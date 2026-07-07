mod commands;
mod discord_presence;
mod events;
mod state;

use croopor_api::app::{
    spawn_background, spawn_benchmark_suite_drivers_resume, spawn_performance_operations_resume,
    spawn_performance_rules_refresh, spawn_remote_flags_refresh, spawn_telemetry_export,
};
use croopor_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use croopor_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use croopor_config::{AppPaths, ConfigStore, InstanceStore};
use croopor_performance::PerformanceManager;
use std::sync::Arc;
use tauri::{Emitter, Manager, WindowEvent};
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
    let telemetry = state.telemetry().clone();
    let discord_presence = discord_presence::spawn(state.clone());
    spawn_performance_operations_resume(&state);
    spawn_benchmark_suite_drivers_resume(&state);
    spawn_performance_rules_refresh(&state);
    spawn_telemetry_export(&state);
    spawn_remote_flags_refresh(&state);
    let close_event_state = state.clone();
    let close_event_presence = discord_presence.clone();
    let desktop_state = state::DesktopState::new(env!("CARGO_PKG_VERSION").to_string());

    let api = match spawn_background(state.clone()).await {
        Ok(api) => api,
        Err(error) => {
            emit_startup_failed(&telemetry);
            discord_presence.shutdown_blocking();
            return Err(Box::new(error));
        }
    };

    info!("desktop shell connected to {}", api.addr);

    let run_result = tauri::Builder::default()
        .manage(desktop_state)
        .manage(state)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::app_restart,
            commands::api_base_url,
            commands::microsoft_sign_in,
            commands::read_skin_file,
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
        .on_window_event(move |window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let window = window.clone();
                let state = close_event_state.clone();
                let discord_presence = close_event_presence.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(error) = commands::close_blocking_error(&state).await {
                        let _ = window.emit(
                            events::DESKTOP_CLOSE_BLOCKED,
                            serde_json::json!({ "error": error }),
                        );
                        return;
                    }
                    commands::flush_pending_saved_skin_applies("window close request", &state)
                        .await;
                    let _ = tokio::task::spawn_blocking(move || {
                        discord_presence.shutdown_blocking();
                    })
                    .await;
                    if let Err(error) = window.destroy() {
                        tracing::warn!("failed to destroy window after close request: {error}");
                    }
                });
            }
        })
        .setup(move |app| {
            app.manage(state::ApiRuntimeState::new(api.addr));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _ = api.task.await;
                let _ = handle.emit(events::DESKTOP_API_STOPPED, serde_json::json!({}));
            });
            Ok(())
        })
        .run(tauri::generate_context!());

    if let Err(error) = run_result {
        emit_startup_failed(&telemetry);
        discord_presence.shutdown_blocking();
        return Err(Box::new(error));
    }

    discord_presence.shutdown_blocking();

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
