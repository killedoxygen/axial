mod auth;
mod bootstrap;
mod discord_presence;
mod lifecycle;
mod native_skin;
#[cfg(debug_assertions)]
mod reset;
mod startup;
#[cfg(target_os = "macos")]
mod termination;
mod update;
mod window;

use bootstrap::DesktopBootstrap;
use lifecycle::DesktopLifecycle;
use tauri::{Emitter, Manager, RunEvent, WindowEvent};

const CLOSE_BLOCKED_EVENT: &str = "axial:desktop:close-blocked";
const API_STOPPED_EVENT: &str = "axial:desktop:api-stopped";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(run())
}

fn app_context() -> tauri::Context<tauri::Wry> {
    // The macro embeds platform symbols, so keep a single expansion.
    tauri::generate_context!()
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tauri::async_runtime::set(tokio::runtime::Handle::current());
    let mut context = app_context();
    if context.config().identifier != bootstrap::APPLICATION_ID {
        return Err(std::io::Error::other(
            "The desktop rewrite must use its independent application identity.",
        )
        .into());
    }
    let dev_origin = context
        .config()
        .build
        .dev_url
        .as_ref()
        .map(|url| url.origin().ascii_serialization());
    let mut window_configs = context
        .config()
        .app
        .windows
        .iter()
        .filter(|config| config.label == window::MAIN_WINDOW);
    let window_config = window_configs
        .next()
        .cloned()
        .ok_or_else(|| std::io::Error::other("The main desktop window is missing."))?;
    if window_configs.next().is_some()
        || window_config.create
        || context.config().app.windows.len() != 1
    {
        return Err(std::io::Error::other(
            "The main desktop window must have one manually created configuration.",
        )
        .into());
    }

    let services = match axial_api::start_desktop(dev_origin.as_deref()).await {
        Ok(services) => services,
        Err(error) => {
            let message = startup::report(context, error).await;
            return Err(std::io::Error::other(message).into());
        }
    };
    let presence = match discord_presence::PresenceObserver::start(
        services.settings.clone(),
        services.instances.clone(),
        services.sessions.clone(),
    ) {
        Ok(presence) => presence,
        Err(error) => {
            tracing::error!(%error, "Could not start desktop presence; settling application services");
            lifecycle::shutdown_server_after_failure(&services.server).await;
            let message = startup::report(context, error.into()).await;
            return Err(std::io::Error::other(message).into());
        }
    };
    let skin_files =
        native_skin::NativeSkinFiles::new(services.library.clone(), services.tasks.clone());
    let lifecycle = DesktopLifecycle::new(
        services.tasks.clone(),
        services.server.clone(),
        presence,
        skin_files.clone(),
        services.skins.clone(),
    );
    #[cfg(debug_assertions)]
    let reset = match reset::NativeReset::new(services.library.clone(), services.tasks.clone()) {
        Ok(reset) => reset,
        Err(error) => {
            lifecycle.shutdown_after_event_loop().await;
            let message = startup::report(context, error.into()).await;
            return Err(std::io::Error::other(message).into());
        }
    };
    let transport = services.server.bootstrap();
    if let Err(error) = bootstrap::confine_content_policy(context.config_mut(), &transport.base_url)
    {
        tracing::error!(%error, "Desktop transport policy failed; settling application services");
        lifecycle.shutdown_after_event_loop().await;
        return Err(error.into());
    }
    let bootstrap = match DesktopBootstrap::new(transport, dev_origin.as_deref()) {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            tracing::error!(%error, "Desktop bootstrap failed; settling application services");
            lifecycle.shutdown_after_event_loop().await;
            return Err(std::io::Error::other(error).into());
        }
    };
    let webview_directory = services.profile_root.join("webview");
    if let Err(error) = tokio::fs::create_dir_all(&webview_directory).await {
        tracing::error!(%error, "Could not prepare desktop storage; settling application services");
        lifecycle.shutdown_after_event_loop().await;
        return Err(error.into());
    }

    let setup_bootstrap = bootstrap.clone();
    let setup_server = services.server.clone();
    let setup_lifecycle = lifecycle.clone();
    let setup_updates = services.updates.clone();
    let setup_tasks = services.tasks.clone();
    let event_lifecycle = lifecycle.clone();
    let event_bootstrap = bootstrap.clone();
    let event_skin_files = skin_files.clone();
    let api_observer = std::sync::Arc::new(std::sync::Mutex::new(None));
    let setup_api_observer = api_observer.clone();
    let builder = tauri::Builder::default()
        .manage(auth::NativeSignIn::new(
            services.auth.clone(),
            services.profile_root.join("oauth-webview"),
        ))
        .manage(bootstrap)
        .manage(lifecycle.clone())
        .manage(skin_files)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            bootstrap::app_version,
            auth::microsoft_sign_in,
            native_skin::pick_skin_file,
            native_skin::consume_skin_drop,
            bootstrap::api_transport_bootstrap,
            window::desktop_chrome,
            window::window_minimize,
            window::window_toggle_maximize,
            window::window_is_maximized,
            window::window_start_dragging,
            window::window_set_resize_background,
            lifecycle::window_close,
            lifecycle::app_restart,
            lifecycle::pending_interface_preferences,
            lifecycle::complete_interface_preferences,
            #[cfg(debug_assertions)]
            reset::app_reset,
        ]);
    #[cfg(debug_assertions)]
    let builder = builder.manage(reset.clone());
    let app = builder
        .on_window_event(move |window, event| {
            if window.label() != crate::window::MAIN_WINDOW || event_lifecycle.exit_allowed() {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                request_close(window.app_handle().clone(), event_lifecycle.clone());
            } else if let WindowEvent::DragDrop(event) = event {
                if window
                    .app_handle()
                    .get_webview_window(crate::window::MAIN_WINDOW)
                    .is_some_and(|webview| {
                        crate::window::require_main_window(&webview, &event_bootstrap).is_ok()
                    })
                {
                    event_skin_files.handle_drag(window, event);
                }
            }
        })
        .setup(move |app| {
            #[cfg(debug_assertions)]
            if app
                .add_capability(
                    tauri::ipc::CapabilityBuilder::new("development-reset")
                        .window(window::MAIN_WINDOW)
                        .permission("allow-app-reset"),
                )
                .is_err()
            {
                lifecycle::report_window_startup_failure(app.handle().clone(), setup_lifecycle);
                return Ok(());
            }
            update::configure(
                app.handle(),
                setup_updates,
                setup_tasks,
                setup_lifecycle.clone(),
            );
            if window::build_main_window(
                app.handle(),
                &window_config,
                webview_directory,
                setup_bootstrap,
            )
            .is_err()
            {
                // Tauri panics if setup returns Err. Preserve application work
                // in this event loop while native error UI reports the failure.
                lifecycle::report_window_startup_failure(app.handle().clone(), setup_lifecycle);
                return Ok(());
            }
            let handle = app.handle().clone();
            let mut preferences = setup_lifecycle.interface_preferences_events();
            let observer = tokio::spawn(async move {
                let waiting = setup_server.wait();
                tokio::pin!(waiting);
                loop {
                    let event = preferences.borrow_and_update().clone();
                    if let Some(event) = event {
                        if handle.emit_to(window::MAIN_WINDOW, lifecycle::PREFERENCES_EVENT, &event).is_err() {
                            setup_lifecycle.interface_preferences_delivery_failed(&event);
                        }
                    }
                    tokio::select! {
                        result = &mut waiting => {
                            if result.is_err() { tracing::error!("The embedded API stopped unexpectedly."); }
                            break;
                        }
                        changed = preferences.changed() => {
                            if changed.is_err() { break; }
                        }
                    }
                }
                let _ = handle.emit_to(
                    window::MAIN_WINDOW,
                    API_STOPPED_EVENT,
                    serde_json::json!({}),
                );
            });
            *setup_api_observer
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(observer);
            Ok(())
        })
        .build(context);
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            tracing::error!(%error, "Could not build the desktop shell; settling application services");
            lifecycle.shutdown_after_event_loop().await;
            return Err(error.into());
        }
    };
    #[cfg(target_os = "macos")]
    let termination = match termination::install(app.handle()) {
        Ok(guard) => guard,
        Err(error) => {
            lifecycle.shutdown_after_event_loop().await;
            return Err(error.into());
        }
    };
    let restart_environment = app.env();
    let exit_lifecycle = lifecycle.clone();
    let exit_code = app.run_return(move |app, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            if !exit_lifecycle.exit_allowed() {
                api.prevent_exit();
                request_close(app.clone(), exit_lifecycle.clone());
            }
        }
    });
    #[cfg(target_os = "macos")]
    drop(termination);
    lifecycle.shutdown_after_event_loop().await;
    #[cfg(debug_assertions)]
    reset.quiesce_after_exit().await;
    let observer = api_observer
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(observer) = observer {
        let _ = observer.await;
    }
    while let Err(error) = lifecycle.release_services_after_exit() {
        tracing::warn!(%error, "Native services remain retained after event-loop shutdown");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    #[cfg(debug_assertions)]
    let pending_reset = reset.take_after_exit();
    let restart_after_exit = lifecycle.restart_after_exit();
    // Managed native facades may outlive the event loop. Their shared service
    // references are now released; drop main's remaining owners before reset.
    drop(lifecycle);
    drop(services);
    #[cfg(debug_assertions)]
    {
        drop(reset);
        if let Some(pending) = pending_reset {
            startup::complete_reset(pending).await;
            tauri::process::restart(&restart_environment);
        }
    }
    if exit_code != 0 {
        return Err(std::io::Error::other(
            "The desktop shell could not start. Application work has settled.",
        )
        .into());
    }
    if restart_after_exit {
        tauri::process::restart(&restart_environment);
    }
    Ok(())
}

fn request_close(app: tauri::AppHandle, lifecycle: DesktopLifecycle) {
    tokio::spawn(async move {
        if let Err(error) = lifecycle::request_window_close(app.clone(), lifecycle).await {
            let _ = app.emit_to(
                window::MAIN_WINDOW,
                CLOSE_BLOCKED_EVENT,
                serde_json::json!({ "error": error }),
            );
        }
    });
}
