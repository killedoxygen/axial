mod commands;
mod events;
mod state;

use croopor_api::app::{
    spawn_background, spawn_benchmark_suite_drivers_resume, spawn_performance_operations_resume,
    spawn_performance_rules_refresh,
};
use croopor_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use croopor_config::{AppPaths, ConfigStore, InstanceStore};
use croopor_performance::PerformanceManager;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        app_name: "Croopor".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config,
        instances,
        installs,
        sessions,
        performance,
        startup_warnings,
        frontend_dir: croopor_api::app::default_frontend_dir(),
    });
    spawn_performance_operations_resume(&state);
    spawn_benchmark_suite_drivers_resume(&state);
    spawn_performance_rules_refresh(&state);
    let desktop_state = state::DesktopState::new(env!("CARGO_PKG_VERSION").to_string());

    let api = spawn_background(state.clone()).await?;

    info!("desktop shell connected to {}", api.addr);

    tauri::Builder::default()
        .manage(desktop_state)
        .manage(state)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::app_restart,
            commands::api_base_url,
            commands::start_install_events,
            commands::start_loader_install_events,
            commands::start_launch_events,
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
            commands::window_is_maximized,
            commands::window_start_dragging,
            commands::window_set_resize_background
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
        .run(tauri::generate_context!())?;

    Ok(())
}
