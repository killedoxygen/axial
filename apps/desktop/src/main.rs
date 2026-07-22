mod commands;
mod discord_presence;
mod events;
mod smoke;
mod state;

use axial_api::app::{spawn_background, start_application_background_workflows};
use axial_api::bootstrap::{
    desktop_app_root_selection_from_environment, open_app_root_session, resolve_app_paths,
};
use axial_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use axial_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use axial_config::{ConfigStore, InstanceStore};
use axial_performance::PerformanceManager;
use std::sync::Arc;
use tauri::{Emitter, Manager, WebviewWindowBuilder, WindowEvent};
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
    let webview_data_directory = smoke::webview_data_directory()?;
    let mut context = tauri::generate_context!();
    let isolated_main_window = smoke::isolate_main_window(
        &mut context.config_mut().app.windows,
        webview_data_directory,
    )?;
    tracing_subscriber::fmt::init();

    let paths = resolve_app_paths(desktop_app_root_selection_from_environment(
        context.config().identifier.as_str(),
    )?)?;
    let root_session = open_app_root_session(&paths)?;
    let root_session = Arc::new(root_session);
    let config_paths = paths.clone();
    let config_root_session = Arc::clone(&root_session);
    let config_startup = tokio::task::spawn_blocking(move || {
        ConfigStore::load_for_startup(config_paths, config_root_session)
    })
    .await??;
    let instance_paths = paths.clone();
    let instance_root_session = Arc::clone(&root_session);
    let instance_startup = tokio::task::spawn_blocking(move || {
        InstanceStore::load_for_startup(instance_paths, instance_root_session)
    })
    .await??;
    let mut startup_warnings = config_startup.warnings;
    startup_warnings.extend(instance_startup.warnings);
    let config = Arc::new(config_startup.store);
    let instances = Arc::new(instance_startup.store);
    let installs = Arc::new(InstallStore::new());
    let sessions = Arc::new(SessionStore::new());
    drop(root_session.prepare_performance_directory()?);
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
    let telemetry = state.telemetry().clone();
    let discord_presence = discord_presence::spawn(state.clone());
    let close_event_state = state.clone();
    let close_event_presence = discord_presence.clone();
    let desktop_state = state::DesktopState::new(env!("CARGO_PKG_VERSION").to_string());
    let close_event_desktop = desktop_state.clone();

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
            commands::app_reset,
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
                let desktop = close_event_desktop.clone();
                let discord_presence = close_event_presence.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = commands::request_window_close(
                        window.app_handle().clone(),
                        state,
                        api,
                        desktop,
                    )
                    .await
                    {
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
                });
            }
        })
        .setup(move |app| {
            app.manage(setup_api_runtime.clone());
            if let Some(window) = isolated_main_window {
                WebviewWindowBuilder::from_config(app.handle(), &window.config)?
                    .data_directory(window.data_directory)
                    .build()?;
            }
            let handle = app.handle().clone();
            let api = setup_api_runtime.clone();
            tauri::async_runtime::spawn(async move {
                let _ = api.wait().await;
                let _ = handle.emit(events::DESKTOP_API_STOPPED, serde_json::json!({}));
            });
            Ok(())
        })
        .run(context);

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
