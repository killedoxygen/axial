mod commands;
mod discord_presence;
mod events;
mod state;

use axial_api::app::{
    spawn_background, spawn_benchmark_suite_drivers_resume, spawn_performance_operations_resume,
    spawn_performance_rules_refresh, spawn_remote_flags_refresh, spawn_telemetry_export,
};
use axial_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use axial_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use axial_config::{AppPaths, ConfigStore, InstanceStore};
use axial_performance::PerformanceManager;
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
    let state = AppState::load(AppStateInit {
        app_name: "Axial".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config,
        instances,
        installs,
        sessions,
        performance,
        startup_warnings,
        frontend_dir: axial_api::app::default_frontend_dir(),
    })
    .await;
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
            if let Err(shutdown_error) = commands::prepare_for_exit(&state).await {
                tracing::warn!(
                    error = shutdown_error,
                    "application shutdown remained incomplete after embedded API startup failed"
                );
            }
            return Err(Box::new(error));
        }
    };
    let api_runtime = state::ApiRuntimeState::new(api);
    let close_event_api = api_runtime.clone();
    let setup_api_runtime = api_runtime.clone();

    info!("desktop shell connected to {}", api_runtime.addr());

    let run_result = tauri::Builder::default()
        .manage(desktop_state)
        .manage(state.clone())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::app_restart,
            commands::api_base_url,
            commands::desktop_chrome,
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
            if window.label() != "main" {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let window = window.clone();
                let state = close_event_state.clone();
                let api = close_event_api.clone();
                let discord_presence = close_event_presence.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(error) = commands::close_blocking_error(&state, &api).await {
                        let _ = window.emit(
                            events::DESKTOP_CLOSE_BLOCKED,
                            serde_json::json!({ "error": error }),
                        );
                        return;
                    }
                    if let Err(error) = commands::prepare_for_exit_with_api(&state, &api).await {
                        let _ = window.emit(
                            events::DESKTOP_CLOSE_BLOCKED,
                            serde_json::json!({ "error": error }),
                        );
                        return;
                    }
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
            app.manage(setup_api_runtime.clone());
            let handle = app.handle().clone();
            let api = setup_api_runtime.clone();
            tauri::async_runtime::spawn(async move {
                let _ = api.wait().await;
                let _ = handle.emit(events::DESKTOP_API_STOPPED, serde_json::json!({}));
            });
            Ok(())
        })
        .run(tauri::generate_context!());

    if let Err(error) = run_result {
        emit_startup_failed(&telemetry);
        discord_presence.shutdown_blocking();
        if let Err(shutdown_error) = commands::prepare_for_exit_with_api(&state, &api_runtime).await
        {
            tracing::warn!(
                error = shutdown_error,
                "application shutdown remained incomplete after the desktop event loop failed"
            );
        }
        return Err(Box::new(error));
    }

    discord_presence.shutdown_blocking();
    commands::prepare_for_exit_with_api(&state, &api_runtime)
        .await
        .map_err(std::io::Error::other)?;

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
