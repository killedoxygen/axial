use crate::events;
use crate::state::{ApiRuntimeState, DesktopState};
use croopor_api::state::{AppState, LaunchEvent, LaunchSessionRecord, LaunchStatusEvent};
use croopor_launcher::{LaunchFailureClass, LaunchState};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn app_version(state: State<'_, DesktopState>) -> String {
    state.version().to_string()
}

#[tauri::command]
pub fn api_base_url(state: State<'_, ApiRuntimeState>) -> String {
    format!("http://{}", state.addr())
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
        pid: record.pid,
        exit_code: record.exit_code,
        failure_class: record
            .failure
            .as_ref()
            .map(|failure| failure_class_name(failure.class).to_string()),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.clone()),
        healing: record.healing.clone(),
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
        LaunchState::Starting => "starting",
        LaunchState::Monitoring => "monitoring",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

fn failure_class_name(class: LaunchFailureClass) -> &'static str {
    match class {
        LaunchFailureClass::Unknown => "unknown",
        LaunchFailureClass::JvmUnsupportedOption => "jvm_unsupported_option",
        LaunchFailureClass::JvmExperimentalUnlock => "jvm_experimental_unlock_required",
        LaunchFailureClass::JvmOptionOrdering => "jvm_option_ordering",
        LaunchFailureClass::JavaRuntimeMismatch => "java_runtime_mismatch",
        LaunchFailureClass::ClasspathModuleConflict => "classpath_or_module_conflict",
        LaunchFailureClass::AuthModeIncompatible => "auth_mode_incompatible",
        LaunchFailureClass::LoaderBootstrapFailure => "loader_bootstrap_failure",
        LaunchFailureClass::StartupStalled => "startup_stalled",
    }
}
