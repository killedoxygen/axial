mod commands;
mod discord_presence;
mod events;
mod native_skin;
mod physical_work;
mod smoke;
mod state;

use axial_api::app::spawn_background_for_origin;
use axial_api::bootstrap::{
    ApplicationLoadRequest, desktop_app_root_selection_from_environment, load_application,
};
use axial_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use axial_resource::PhysicalIoClass;
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
    let dev_origin = context
        .config()
        .build
        .dev_url
        .as_ref()
        .map(|url| url.origin().ascii_serialization());
    let main_window_dev_origin = dev_origin.clone();
    tracing_subscriber::fmt::init();

    let loaded = load_application(ApplicationLoadRequest {
        root: desktop_app_root_selection_from_environment(context.config().identifier.as_str())?,
        app_name: "Axial".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .await?;
    let state = loaded.state;
    tracing::debug!(health = ?loaded.health, "application startup settled");
    let telemetry = state.telemetry().clone();
    let discord_presence = discord_presence::spawn(state.clone());
    let close_event_state = state.clone();
    let close_event_presence = discord_presence.clone();
    let desktop_state = state::DesktopState::new(env!("CARGO_PKG_VERSION").to_string());
    let close_event_desktop = desktop_state.clone();

    let api = match spawn_background_for_origin(state.clone(), dev_origin.as_deref()).await {
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
            commands::api_transport_bootstrap,
            commands::desktop_chrome,
            commands::microsoft_sign_in,
            commands::pick_skin_file,
            commands::consume_skin_drop,
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
            match event {
                WindowEvent::DragDrop(event) => native_skin::handle_native_skin_drag(
                    window,
                    close_event_desktop.native_skin_drop().clone(),
                    Arc::clone(close_event_state.root_session()),
                    event,
                ),
                WindowEvent::CloseRequested { api, .. } => {
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
                        let _ = physical_work::run(PhysicalIoClass::Metadata, 0, move || {
                            discord_presence.shutdown_blocking();
                        })
                        .await;
                    });
                }
                _ => {}
            }
        })
        .setup(move |app| {
            app.manage(setup_api_runtime.clone());
            if let Some(window) = isolated_main_window {
                let allowed_dev_origin = main_window_dev_origin.clone();
                WebviewWindowBuilder::from_config(app.handle(), &window.config)?
                    .data_directory(window.data_directory)
                    .on_navigation(move |url| {
                        main_window_navigation_allowed(url, allowed_dev_origin.as_deref())
                    })
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

fn main_window_navigation_allowed(url: &tauri::Url, dev_origin: Option<&str>) -> bool {
    matches!(
        (url.scheme(), url.host_str(), url.port()),
        ("tauri", Some("localhost"), None) | ("http", Some("tauri.localhost"), None)
    ) || dev_origin.is_some_and(|origin| url.origin().ascii_serialization() == origin)
}

fn emit_startup_failed(telemetry: &Arc<TelemetryHub>) {
    telemetry.emit_sync_best_effort(TelemetryEvent::error_captured(
        TelemetryErrorKind::StartupFailed,
        TelemetryErrorArea::Startup,
        TelemetryErrorLevel::Error,
        "Backend startup failed.",
    ));
}
