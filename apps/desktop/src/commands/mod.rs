use crate::events;
use crate::native_skin::{NativeSkinFile, NativeSkinFileAdmission};
use crate::state::{
    ApiRuntimeState, DesktopState, TerminalAttemptOwner, TerminalFailure, TerminalIntent,
    TerminalResult,
};
use axial_api::application::launch::public_launch_status;
use axial_api::application::{
    public_loader_install_progress_record_json, public_vanilla_install_progress_record_json,
};
use axial_api::state::{AppState, LaunchEvent};
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::webview::Color;
use tauri::{
    AppHandle, Emitter, Manager, State, UserAttentionType, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_dialog::DialogExt as _;

const RESTART_BUSY_MESSAGE: &str = "Restart is blocked while installs or launches are active.";
const CLOSE_BUSY_MESSAGE: &str = "Close is blocked while installs or launches are active.";
const API_CLOSE_FAILED_MESSAGE: &str =
    "Close is blocked because the local API did not stop cleanly.";
const STATE_CLOSE_FAILED_MESSAGE: &str =
    "Close is blocked because application shutdown is incomplete.";
const TERMINAL_CONFLICT_MESSAGE: &str = "Another desktop shutdown action is already in progress.";
const RESET_UNAVAILABLE_MESSAGE: &str = "Developer reset is unavailable in this build.";
const RESET_PREFLIGHT_FAILED_MESSAGE: &str =
    "Reset is blocked because launcher-owned storage could not be proven safe.";
const RESET_DELETE_FAILED_MESSAGE: &str =
    "Reset is incomplete because launcher-owned data could not be deleted. Try again.";
const WINDOW_CLOSE_FAILED_MESSAGE: &str = "Close is blocked because the window could not close.";
const MICROSOFT_SIGN_IN_WINDOW_LABEL: &str = "microsoft-signin";
const MICROSOFT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct NativeMicrosoftSignIn {
    status: &'static str,
    login_id: Option<String>,
    profile_name: Option<String>,
    owns_minecraft_java: Option<bool>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct NativeDesktopChrome {
    platform: &'static str,
    chrome_mode: &'static str,
}

#[tauri::command]
pub fn app_version(state: State<'_, DesktopState>) -> String {
    state.version().to_string()
}

#[tauri::command]
pub fn desktop_chrome() -> NativeDesktopChrome {
    NativeDesktopChrome {
        platform: std::env::consts::OS,
        chrome_mode: desktop_chrome_mode(),
    }
}

#[cfg(target_os = "macos")]
fn desktop_chrome_mode() -> &'static str {
    "mac-overlay"
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn desktop_chrome_mode() -> &'static str {
    "custom-frameless"
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn desktop_chrome_mode() -> &'static str {
    "native-decorated"
}

#[tauri::command]
pub fn api_base_url(state: State<'_, ApiRuntimeState>) -> String {
    format!("http://{}", state.addr())
}

#[tauri::command]
pub async fn microsoft_sign_in(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<NativeMicrosoftSignIn, String> {
    let flow = axial_api::microsoft_auth::begin_login()
        .await
        .map_err(|error| error.user_message())?;
    let url = flow
        .auth_request_uri()
        .parse()
        .map_err(|_| "Microsoft sign-in returned an invalid URL.".to_string())?;

    if let Some(window) = app.get_webview_window(MICROSOFT_SIGN_IN_WINDOW_LABEL) {
        let _ = window.close();
    }

    let window = WebviewWindowBuilder::new(
        &app,
        MICROSOFT_SIGN_IN_WINDOW_LABEL,
        WebviewUrl::External(url),
    )
    .title("Sign in with Microsoft")
    .inner_size(520.0, 720.0)
    .resizable(true)
    .center()
    .build()
    .map_err(|error| format!("Could not open Microsoft sign-in window: {error}"))?;

    let _ = window.request_user_attention(Some(UserAttentionType::Informational));
    let start = Instant::now();

    while start.elapsed() < MICROSOFT_SIGN_IN_TIMEOUT {
        if window.title().is_err() {
            return Ok(microsoft_sign_in_cancelled());
        }

        if let Ok(url) = window.url()
            && url
                .as_str()
                .starts_with(axial_api::microsoft_auth::MICROSOFT_AUTH_REDIRECT_URL)
        {
            if let Some(code) = axial_api::microsoft_auth::redirect_code_from_url(&url) {
                let _ = window.close();
                let outcome =
                    axial_api::microsoft_auth::finish_login(flow, &code, state.auth_logins())
                        .await
                        .map_err(|error| error.user_message())?;
                return Ok(NativeMicrosoftSignIn {
                    status: "authenticated",
                    login_id: Some(outcome.login_id),
                    profile_name: Some(outcome.profile_name),
                    owns_minecraft_java: Some(outcome.owns_minecraft_java),
                });
            }

            let _ = window.close();
            return Err("Microsoft sign-in was cancelled or rejected.".to_string());
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = window.close();
    Err("Microsoft sign-in timed out.".to_string())
}

fn microsoft_sign_in_cancelled() -> NativeMicrosoftSignIn {
    NativeMicrosoftSignIn {
        status: "cancelled",
        login_id: None,
        profile_name: None,
        owns_minecraft_java: None,
    }
}

#[tauri::command]
pub async fn pick_skin_file(
    app: AppHandle,
    state: State<'_, AppState>,
    desktop: State<'_, DesktopState>,
) -> Result<Option<NativeSkinFile>, String> {
    desktop.native_skin_drop().ensure_ingress_open()?;
    let (selected_tx, selected_rx) = tokio::sync::oneshot::channel();
    let root_session = Arc::clone(state.root_session());
    let native_skin_drop = desktop.native_skin_drop().clone();
    app.dialog()
        .file()
        .add_filter("PNG skin", &["png"])
        .pick_file(move |selected| {
            let admission = selected
                .map(|selected| -> Result<_, String> {
                    let path = selected
                        .into_path()
                        .map_err(|_| "Native skin picker returned an invalid file.".to_string())?;
                    let ingress_permit = native_skin_drop.try_begin_ingress()?;
                    let admission = NativeSkinFileAdmission::admit(&root_session, path)?;
                    Ok((admission, ingress_permit))
                })
                .transpose();
            let _ = selected_tx.send(admission);
        });
    let selected = selected_rx
        .await
        .map_err(|_| "Native skin picker stopped before returning a selection.".to_string())?;
    let Some((admission, ingress_permit)) = selected? else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _ingress_permit = ingress_permit;
        admission.read()
    })
        .await
        .map_err(|_| "Could not read skin file.".to_string())?
        .map(Some)
}

#[tauri::command]
pub async fn consume_skin_drop(
    token: String,
    state: State<'_, DesktopState>,
) -> Result<NativeSkinFile, String> {
    let coordinator = state.native_skin_drop().clone();
    tauri::async_runtime::spawn_blocking(move || coordinator.consume(&token))
        .await
        .map_err(|_| "Could not read dropped skin file.".to_string())?
}

#[tauri::command]
pub async fn app_restart(
    app: AppHandle,
    state: State<'_, AppState>,
    api: State<'_, ApiRuntimeState>,
    desktop: State<'_, DesktopState>,
) -> Result<(), String> {
    if !desktop.terminal_is_claimed(TerminalIntent::Restart) {
        let active_installs = state.installs().active_install_count().await;
        let active_sessions = state.sessions().active_session_count().await;
        restart_readiness(active_installs, active_sessions)?;
    }
    let start = desktop
        .begin_terminal(TerminalIntent::Restart)
        .map_err(|_| TERMINAL_CONFLICT_MESSAGE.to_string())?;
    if let Some(owner) = start.owner {
        let state = state.inner().clone();
        let api = api.inner().clone();
        spawn_terminal_owner(owner, async move {
            prepare_terminal_exit_with_api(&state, &api).await?;
            app.request_restart();
            Ok(())
        });
    }
    start.attempt.wait().await.map_err(terminal_error_message)
}

#[tauri::command]
pub async fn app_reset(
    app: AppHandle,
    state: State<'_, AppState>,
    api: State<'_, ApiRuntimeState>,
    desktop: State<'_, DesktopState>,
) -> Result<(), String> {
    if !cfg!(debug_assertions) {
        return Err(RESET_UNAVAILABLE_MESSAGE.to_string());
    }

    let start = desktop
        .begin_terminal(TerminalIntent::Reset)
        .map_err(|_| TERMINAL_CONFLICT_MESSAGE.to_string())?;
    if let Some(owner) = start.owner {
        let state = state.inner().clone();
        let api = api.inner().clone();
        let root_session = state.root_session().clone();
        spawn_terminal_owner(owner, async move {
            let reset_paths = state.config().paths().clone();
            let reset_config = state.config().current();
            root_session
                .reset_preflight(&reset_paths, &reset_config)
                .map_err(|_| TerminalFailure::ResetPreflight)?;
            prepare_terminal_exit_with_api(&state, &api).await?;
            let reset_authority = root_session
                .begin_reset()
                .await
                .map_err(|_| TerminalFailure::ResetDeletion)?;
            let cleared_root = clear_owned_root_off_runtime(reset_authority).await?;
            if let Err(_receipt) = cleared_root.release() {
                std::process::abort();
            }
            app.request_restart();
            Ok(())
        });
    }
    start.attempt.wait().await.map_err(terminal_error_message)
}

fn restart_readiness(active_installs: usize, active_sessions: usize) -> Result<(), String> {
    activity_readiness(active_installs, active_sessions, RESTART_BUSY_MESSAGE)
}

fn close_readiness(active_installs: usize, active_sessions: usize) -> Result<(), String> {
    activity_readiness(active_installs, active_sessions, CLOSE_BUSY_MESSAGE)
}

fn activity_readiness(
    active_installs: usize,
    active_sessions: usize,
    busy_message: &str,
) -> Result<(), String> {
    if active_installs > 0 || active_sessions > 0 {
        Err(busy_message.to_string())
    } else {
        Ok(())
    }
}

#[tauri::command]
pub fn window_minimize(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_toggle_maximize(app: AppHandle) -> Result<bool, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    let maximized = window.is_maximized().map_err(|e| e.to_string())?;
    if maximized {
        window.unmaximize().map_err(|e| e.to_string())?;
        Ok(false)
    } else {
        window.maximize().map_err(|e| e.to_string())?;
        Ok(true)
    }
}

#[tauri::command]
pub async fn window_close(
    app: AppHandle,
    state: State<'_, AppState>,
    api: State<'_, ApiRuntimeState>,
    desktop: State<'_, DesktopState>,
) -> Result<(), String> {
    request_window_close(
        app,
        state.inner().clone(),
        api.inner().clone(),
        desktop.inner().clone(),
    )
    .await
}

pub async fn request_window_close(
    app: AppHandle,
    state: AppState,
    api: ApiRuntimeState,
    desktop: DesktopState,
) -> Result<(), String> {
    if !desktop.terminal_is_claimed(TerminalIntent::Close) {
        let active_installs = state.installs().active_install_count().await;
        let active_sessions = state.sessions().active_session_count().await;
        close_readiness(active_installs, active_sessions)?;
    }
    let start = desktop
        .begin_terminal(TerminalIntent::Close)
        .map_err(|_| TERMINAL_CONFLICT_MESSAGE.to_string())?;
    if let Some(owner) = start.owner {
        spawn_terminal_owner(owner, async move {
            prepare_terminal_exit_with_api(&state, &api).await?;
            let window = app
                .get_webview_window("main")
                .ok_or(TerminalFailure::WindowClose)?;
            window.destroy().map_err(|_| TerminalFailure::WindowClose)?;
            Ok(())
        });
    }
    start.attempt.wait().await.map_err(terminal_error_message)
}

pub async fn prepare_for_exit(state: &AppState) -> Result<(), String> {
    state
        .shutdown()
        .await
        .map_err(|_| STATE_CLOSE_FAILED_MESSAGE.to_string())
}

pub async fn prepare_for_exit_with_api(
    state: &AppState,
    api: &ApiRuntimeState,
) -> Result<(), String> {
    prepare_terminal_exit_with_api(state, api)
        .await
        .map_err(terminal_error_message)
}

async fn prepare_terminal_exit_with_api(state: &AppState, api: &ApiRuntimeState) -> TerminalResult {
    let (api_result, state_result) = tokio::join!(api.shutdown(), state.shutdown());
    let api_result = api_result.map_err(|_| TerminalFailure::ApiShutdown);
    let state_result = state_result.map_err(|_| TerminalFailure::AppShutdown);
    api_result?;
    state_result
}

fn spawn_terminal_owner<Work>(owner: TerminalAttemptOwner, work: Work)
where
    Work: Future<Output = TerminalResult> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        owner.wait_for_ingress_drain().await;
        let task = tauri::async_runtime::spawn(work);
        let result = task.await.unwrap_or(Err(TerminalFailure::OwnerStopped));
        owner.finish(result);
    });
}

fn terminal_error_message(error: TerminalFailure) -> String {
    match error {
        TerminalFailure::ApiShutdown => API_CLOSE_FAILED_MESSAGE,
        TerminalFailure::AppShutdown => STATE_CLOSE_FAILED_MESSAGE,
        TerminalFailure::ResetPreflight => RESET_PREFLIGHT_FAILED_MESSAGE,
        TerminalFailure::ResetDeletion => RESET_DELETE_FAILED_MESSAGE,
        TerminalFailure::WindowClose => WINDOW_CLOSE_FAILED_MESSAGE,
        TerminalFailure::OwnerStopped => STATE_CLOSE_FAILED_MESSAGE,
    }
    .to_string()
}

async fn clear_owned_root_off_runtime(
    authority: axial_config::AppRootResetAuthority,
) -> Result<axial_config::AppRootClearReceipt, TerminalFailure> {
    tauri::async_runtime::spawn_blocking(move || authority.clear_owned_root())
        .await
        .map_err(|_| TerminalFailure::ResetDeletion)?
        .map_err(|_| TerminalFailure::ResetDeletion)
}

#[tauri::command]
pub fn window_is_maximized(app: AppHandle) -> Result<bool, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    window.is_maximized().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_start_dragging(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    window.start_dragging().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_set_resize_background(app: AppHandle, dark: bool) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    let color = if dark {
        Color(16, 13, 10, 255)
    } else {
        Color(244, 241, 237, 255)
    };
    window
        .set_background_color(Some(color))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn start_install_events(
    app: AppHandle,
    state: State<'_, AppState>,
    desktop: State<'_, DesktopState>,
    install_id: String,
) -> Result<(), String> {
    let (snapshot, mut receiver) = state
        .installs()
        .subscribe_records(&install_id)
        .await
        .ok_or_else(|| "install session not found".to_string())?;
    let event_name = events::install_progress(&install_id);
    let mut owner = desktop.install_events().replace(install_id);

    tauri::async_runtime::spawn(async move {
        if let Some(record) = snapshot.latest {
            let terminal = record.progress.done;
            if owner
                .emit_if_current(|| {
                    app.emit(
                        &event_name,
                        public_vanilla_install_progress_record_json(&record),
                    )
                })
                .ok()
                != Some(true)
            {
                return;
            }
            if terminal {
                return;
            }
        }
        if snapshot.done {
            return;
        }
        loop {
            let event = tokio::select! {
                biased;
                _ = owner.cancelled() => return,
                event = receiver.recv() => event,
            };
            match event {
                Ok(record) => {
                    let terminal = record.progress.done;
                    if owner
                        .emit_if_current(|| {
                            app.emit(
                                &event_name,
                                public_vanilla_install_progress_record_json(&record),
                            )
                        })
                        .ok()
                        != Some(true)
                    {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn start_loader_install_events(
    app: AppHandle,
    state: State<'_, AppState>,
    desktop: State<'_, DesktopState>,
    install_id: String,
) -> Result<(), String> {
    let (snapshot, mut receiver) = state
        .installs()
        .subscribe_records(&install_id)
        .await
        .ok_or_else(|| "loader install session not found".to_string())?;
    let event_name = events::loader_install_progress(&install_id);
    let mut owner = desktop.loader_install_events().replace(install_id);

    tauri::async_runtime::spawn(async move {
        if let Some(record) = snapshot.latest {
            let terminal = record.progress.done;
            if owner
                .emit_if_current(|| {
                    app.emit(
                        &event_name,
                        public_loader_install_progress_record_json(&record),
                    )
                })
                .ok()
                != Some(true)
            {
                return;
            }
            if terminal {
                return;
            }
        }
        if snapshot.done {
            return;
        }
        loop {
            let event = tokio::select! {
                biased;
                _ = owner.cancelled() => return,
                event = receiver.recv() => event,
            };
            match event {
                Ok(record) => {
                    let terminal = record.progress.done;
                    if owner
                        .emit_if_current(|| {
                            app.emit(
                                &event_name,
                                public_loader_install_progress_record_json(&record),
                            )
                        })
                        .ok()
                        != Some(true)
                    {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn start_launch_events(
    app: AppHandle,
    state: State<'_, AppState>,
    desktop: State<'_, DesktopState>,
    session_id: String,
) -> Result<(), String> {
    let mut owner = desktop.launch_events().replace(session_id.clone());
    let mut subscription = state
        .sessions()
        .subscribe_events(&session_id)
        .await
        .ok_or_else(|| "session not found".to_string())?;
    let status_event_name = events::launch_status(&session_id);
    let log_event_name = events::launch_log(&session_id);

    tauri::async_runtime::spawn(async move {
        let status = public_launch_status(subscription.retained_status());
        let mut last_revision = status.revision;
        let terminal = status.view_model.terminal;
        if owner
            .emit_if_current(|| app.emit(&status_event_name, status))
            .ok()
            != Some(true)
        {
            return;
        }
        if terminal {
            return;
        }
        loop {
            let event = tokio::select! {
                biased;
                _ = owner.cancelled() => return,
                event = subscription.recv() => event,
            };
            match event {
                Ok(LaunchEvent::Status(status)) => {
                    if status.revision <= last_revision {
                        continue;
                    }
                    let status = public_launch_status(&status);
                    last_revision = status.revision;
                    let terminal = status.view_model.terminal;
                    if owner
                        .emit_if_current(|| app.emit(&status_event_name, status))
                        .ok()
                        != Some(true)
                    {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
                Ok(LaunchEvent::Log(log)) => {
                    if owner
                        .emit_if_current(|| app.emit(&log_event_name, log))
                        .ok()
                        != Some(true)
                    {
                        return;
                    }
                }
                Ok(LaunchEvent::ProcessSettled { .. }) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let Some(status) = subscription.rebase().await else {
                        return;
                    };
                    if status.revision <= last_revision {
                        continue;
                    }
                    let status = public_launch_status(&status);
                    last_revision = status.revision;
                    let terminal = status.view_model.terminal;
                    if owner
                        .emit_if_current(|| app.emit(&status_event_name, status))
                        .ok()
                        != Some(true)
                    {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CLOSE_BUSY_MESSAGE, RESTART_BUSY_MESSAGE, close_readiness, restart_readiness,
    };

    #[test]
    fn restart_readiness_allows_idle_app() {
        assert_eq!(restart_readiness(0, 0), Ok(()));
    }

    #[test]
    fn restart_readiness_blocks_active_installs() {
        assert_eq!(
            restart_readiness(1, 0),
            Err(RESTART_BUSY_MESSAGE.to_string())
        );
    }

    #[test]
    fn restart_readiness_blocks_active_sessions() {
        assert_eq!(
            restart_readiness(0, 1),
            Err(RESTART_BUSY_MESSAGE.to_string())
        );
    }

    #[test]
    fn restart_readiness_blocks_mixed_activity() {
        assert_eq!(
            restart_readiness(2, 3),
            Err(RESTART_BUSY_MESSAGE.to_string())
        );
    }

    #[test]
    fn close_readiness_allows_idle_app() {
        assert_eq!(close_readiness(0, 0), Ok(()));
    }

    #[test]
    fn close_readiness_blocks_active_installs() {
        assert_eq!(close_readiness(1, 0), Err(CLOSE_BUSY_MESSAGE.to_string()));
    }

    #[test]
    fn close_readiness_blocks_active_sessions() {
        assert_eq!(close_readiness(0, 1), Err(CLOSE_BUSY_MESSAGE.to_string()));
    }

}
