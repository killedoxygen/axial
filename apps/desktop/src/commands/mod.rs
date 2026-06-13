use crate::events;
use crate::state::{ApiRuntimeState, DesktopState};
use croopor_api::routes::flush_pending_saved_skin_applies_for_shutdown;
use croopor_api::state::{AppState, LaunchEvent, LaunchSessionRecord, LaunchStatusEvent};
use croopor_launcher::LaunchState;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tauri::webview::Color;
use tauri::{AppHandle, Emitter, Manager, State};

const RESTART_BUSY_MESSAGE: &str = "Restart is blocked while installs or launches are active.";
const CLOSE_BUSY_MESSAGE: &str = "Close is blocked while installs or launches are active.";
const SKIN_FILE_MAX_BYTES: u64 = 256 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct NativeSkinFile {
    name: String,
    bytes: Vec<u8>,
}

#[tauri::command]
pub fn app_version(state: State<'_, DesktopState>) -> String {
    state.version().to_string()
}

#[tauri::command]
pub fn api_base_url(state: State<'_, ApiRuntimeState>) -> String {
    format!("http://{}", state.addr())
}

#[tauri::command]
pub async fn read_skin_file(path: String) -> Result<NativeSkinFile, String> {
    tauri::async_runtime::spawn_blocking(move || read_skin_file_from_path(PathBuf::from(path)))
        .await
        .map_err(|err| err.to_string())?
}

fn read_skin_file_from_path(path: PathBuf) -> Result<NativeSkinFile, String> {
    let extension_is_png = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"));
    if !extension_is_png {
        return Err("Choose a PNG skin file.".to_string());
    }

    let metadata = fs::metadata(&path).map_err(|_| "Could not read skin file.".to_string())?;
    if !metadata.is_file() {
        return Err("Choose a PNG skin file.".to_string());
    }
    if metadata.len() > SKIN_FILE_MAX_BYTES {
        return Err("Skin file is too large; choose a PNG under 256 KiB.".to_string());
    }

    let bytes = fs::read(&path).map_err(|_| "Could not read skin file.".to_string())?;
    if bytes.len() as u64 > SKIN_FILE_MAX_BYTES {
        return Err("Skin file is too large; choose a PNG under 256 KiB.".to_string());
    }
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("Choose a PNG skin file.".to_string());
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("skin.png")
        .to_string();

    Ok(NativeSkinFile { name, bytes })
}

#[tauri::command]
pub async fn app_restart(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let active_installs = state.installs().active_install_count().await;
    let active_sessions = state.sessions().active_session_count().await;
    restart_readiness(active_installs, active_sessions)?;
    app.request_restart();
    Ok(())
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
pub async fn window_close(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let active_installs = state.installs().active_install_count().await;
    let active_sessions = state.sessions().active_session_count().await;
    close_readiness(active_installs, active_sessions)?;

    if let Err((status, _)) = flush_pending_saved_skin_applies_for_shutdown(state.inner()).await {
        tracing::warn!(
            "failed to flush pending skin changes before desktop close: HTTP {}",
            status
        );
    }

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    window.close().map_err(|e| e.to_string())
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
    install_id: String,
) -> Result<(), String> {
    let (history, mut receiver, done) = state
        .installs()
        .subscribe(&install_id)
        .await
        .ok_or_else(|| "install session not found".to_string())?;
    let event_name = events::install_progress(&install_id);
    let installs = state.installs().clone();

    tauri::async_runtime::spawn(async move {
        for progress in history {
            let terminal = progress.done;
            let _ = app.emit(&event_name, progress);
            if terminal {
                installs.remove(&install_id).await;
                return;
            }
        }
        if done {
            installs.remove(&install_id).await;
            return;
        }
        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    let terminal = progress.done;
                    let _ = app.emit(&event_name, progress);
                    if terminal {
                        installs.remove(&install_id).await;
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    installs.remove(&install_id).await;
                    return;
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn start_loader_install_events(
    app: AppHandle,
    state: State<'_, AppState>,
    install_id: String,
) -> Result<(), String> {
    let (history, mut receiver, done) = state
        .installs()
        .subscribe(&install_id)
        .await
        .ok_or_else(|| "loader install session not found".to_string())?;
    let event_name = events::loader_install_progress(&install_id);
    let installs = state.installs().clone();

    tauri::async_runtime::spawn(async move {
        for progress in history {
            let terminal = progress.done;
            let _ = app.emit(&event_name, progress);
            if terminal {
                installs.remove(&install_id).await;
                return;
            }
        }
        if done {
            installs.remove(&install_id).await;
            return;
        }
        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    let terminal = progress.done;
                    let _ = app.emit(&event_name, progress);
                    if terminal {
                        installs.remove(&install_id).await;
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    installs.remove(&install_id).await;
                    return;
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn start_launch_events(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let snapshot = state
        .sessions()
        .get(&session_id)
        .await
        .ok_or_else(|| "session not found".to_string())?;
    let mut receiver = state
        .sessions()
        .subscribe(&session_id)
        .await
        .ok_or_else(|| "session not found".to_string())?;
    let status_event_name = events::launch_status(&session_id);
    let log_event_name = events::launch_log(&session_id);

    tauri::async_runtime::spawn(async move {
        let _ = app.emit(&status_event_name, snapshot_status(&snapshot));
        if is_terminal_state(snapshot.state) {
            return;
        }
        loop {
            match receiver.recv().await {
                Ok(LaunchEvent::Status(status)) => {
                    let terminal = matches!(status.state.as_str(), "failed" | "exited");
                    let _ = app.emit(&status_event_name, status);
                    if terminal {
                        return;
                    }
                }
                Ok(LaunchEvent::Log(log)) => {
                    let _ = app.emit(&log_event_name, log);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    Ok(())
}

fn snapshot_status(record: &LaunchSessionRecord) -> LaunchStatusEvent {
    LaunchStatusEvent {
        state: launch_state_name(record.state).to_string(),
        benchmark: record.benchmark.clone(),
        pid: record.pid,
        exit_code: record.exit_code,
        failure_class: record
            .failure
            .as_ref()
            .map(|failure| failure.class.as_str().to_string()),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.clone()),
        healing: record.healing.clone(),
        guardian: record.guardian.clone(),
        stages: record.stages.clone(),
    }
}

fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

fn launch_state_name(state: LaunchState) -> &'static str {
    match state {
        LaunchState::Idle => "idle",
        LaunchState::Queued => "queued",
        LaunchState::Planning => "planning",
        LaunchState::Validating => "validating",
        LaunchState::EnsuringRuntime => "ensuring_runtime",
        LaunchState::DownloadingRuntime => "downloading_runtime",
        LaunchState::Preparing => "preparing",
        LaunchState::Prewarming => "prewarming",
        LaunchState::Starting => "starting",
        LaunchState::Monitoring => "monitoring",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLOSE_BUSY_MESSAGE, PNG_SIGNATURE, RESTART_BUSY_MESSAGE, SKIN_FILE_MAX_BYTES,
        close_readiness, read_skin_file_from_path, restart_readiness,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "croopor-desktop-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        dir
    }

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

    #[test]
    fn read_skin_file_accepts_png_file() {
        let dir = test_dir("read-skin-ok");
        let path = dir.join("player.png");
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(b"smoke");
        fs::write(&path, &png).expect("write png");

        let file = read_skin_file_from_path(path).expect("native skin file");

        assert_eq!(file.name, "player.png");
        assert_eq!(file.bytes, png);
        fs::remove_dir_all(dir).expect("cleanup test dir");
    }

    #[test]
    fn read_skin_file_rejects_non_png_extension() {
        let dir = test_dir("read-skin-extension");
        let path = dir.join("player.txt");
        fs::write(&path, PNG_SIGNATURE).expect("write file");

        let result = read_skin_file_from_path(path);

        assert_eq!(result, Err("Choose a PNG skin file.".to_string()));
        fs::remove_dir_all(dir).expect("cleanup test dir");
    }

    #[test]
    fn read_skin_file_rejects_oversized_png() {
        let dir = test_dir("read-skin-oversized");
        let path = dir.join("large.png");
        fs::write(&path, vec![0; (SKIN_FILE_MAX_BYTES + 1) as usize])
            .expect("write oversized file");

        let result = read_skin_file_from_path(path);

        assert_eq!(
            result,
            Err("Skin file is too large; choose a PNG under 256 KiB.".to_string())
        );
        fs::remove_dir_all(dir).expect("cleanup test dir");
    }
}
