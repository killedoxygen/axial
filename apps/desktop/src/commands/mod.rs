use crate::events;
use crate::state::{
    ApiRuntimeState, DesktopState, TerminalAttemptOwner, TerminalFailure, TerminalIntent,
    TerminalResult,
};
use axial_api::application::launch::{public_launch_status_json, snapshot_status};
use axial_api::application::{
    public_loader_install_progress_json, public_vanilla_install_progress_json,
};
use axial_api::state::{AppState, LaunchEvent};
use axial_launcher::is_terminal_state;
use serde::Serialize;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tauri::webview::Color;
use tauri::{
    AppHandle, Emitter, Manager, State, UserAttentionType, WebviewUrl, WebviewWindowBuilder,
};

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
const SKIN_FILE_MAX_BYTES: u64 = 256 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const MICROSOFT_SIGN_IN_WINDOW_LABEL: &str = "microsoft-signin";
const MICROSOFT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct NativeSkinFile {
    name: String,
    bytes: Vec<u8>,
}

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

#[cfg(target_os = "windows")]
fn desktop_chrome_mode() -> &'static str {
    "custom-frameless"
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
pub async fn app_restart(
    app: AppHandle,
    state: State<'_, AppState>,
    api: State<'_, ApiRuntimeState>,
    desktop: State<'_, DesktopState>,
) -> Result<(), String> {
    if !desktop.terminal().is_claimed(TerminalIntent::Restart) {
        let active_installs = state.installs().active_install_count().await;
        let active_sessions = state.sessions().active_session_count().await;
        restart_readiness(active_installs, active_sessions)?;
    }
    let start = desktop
        .terminal()
        .begin(TerminalIntent::Restart)
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalResetPlan {
    config_root: PathBuf,
    expected_root: ResetRootExpectation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResetRootExpectation {
    Absent,
    Present(ResetRootIdentity),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResetRootIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
    },
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

    let plan = prepare_reset_plan(desktop.paths().clone(), state.config().current()).await?;
    let start = desktop
        .terminal()
        .begin(TerminalIntent::Reset)
        .map_err(|_| TERMINAL_CONFLICT_MESSAGE.to_string())?;
    if let Some(owner) = start.owner {
        let state = state.inner().clone();
        let api = api.inner().clone();
        spawn_terminal_owner(owner, async move {
            prepare_terminal_exit_with_api(&state, &api).await?;
            delete_reset_root_off_runtime(plan).await?;
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
    if !desktop.terminal().is_claimed(TerminalIntent::Close) {
        let active_installs = state.installs().active_install_count().await;
        let active_sessions = state.sessions().active_session_count().await;
        close_readiness(active_installs, active_sessions)?;
    }
    let start = desktop
        .terminal()
        .begin(TerminalIntent::Close)
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

async fn prepare_reset_plan(
    paths: axial_config::AppPaths,
    config: axial_config::AppConfig,
) -> Result<TerminalResetPlan, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let executable = std::env::current_exe().map_err(|_| TerminalFailure::ResetPreflight)?;
        build_reset_plan(&paths, &config, &executable)
    })
    .await
    .map_err(|_| RESET_PREFLIGHT_FAILED_MESSAGE.to_string())?
    .map_err(terminal_error_message)
}

fn build_reset_plan(
    paths: &axial_config::AppPaths,
    config: &axial_config::AppConfig,
    restart_executable: &Path,
) -> Result<TerminalResetPlan, TerminalFailure> {
    let config_root = validate_reset_paths(paths)?;
    if !restart_executable.is_absolute() {
        return Err(TerminalFailure::ResetPreflight);
    }
    let executable =
        fs::symlink_metadata(restart_executable).map_err(|_| TerminalFailure::ResetPreflight)?;
    if !executable.file_type().is_file() {
        return Err(TerminalFailure::ResetPreflight);
    }

    let configured_library = config.library_dir.trim();
    match config.library_mode.as_str() {
        "managed" => {
            if !configured_library.is_empty() && Path::new(configured_library) != paths.library_dir
            {
                return Err(TerminalFailure::ResetPreflight);
            }
        }
        "existing" => {
            if configured_library.is_empty()
                || path_resolves_within(Path::new(configured_library), &paths.config_dir)
                    .map_err(|_| TerminalFailure::ResetPreflight)?
            {
                return Err(TerminalFailure::ResetPreflight);
            }
        }
        _ => return Err(TerminalFailure::ResetPreflight),
    }

    Ok(TerminalResetPlan {
        expected_root: capture_reset_root(&config_root)?,
        config_root,
    })
}

fn validate_reset_paths(paths: &axial_config::AppPaths) -> Result<PathBuf, TerminalFailure> {
    if paths.config_dir.as_os_str().is_empty()
        || paths.config_file != paths.config_dir.join("config.json")
        || paths.instances_file != paths.config_dir.join("instances.json")
        || paths.instances_dir != paths.config_dir.join("instances")
        || paths.music_dir != paths.config_dir.join("music")
        || paths.library_dir != paths.config_dir.join("library")
    {
        return Err(TerminalFailure::ResetPreflight);
    }
    let root = absolute_lexical(&paths.config_dir).map_err(|_| TerminalFailure::ResetPreflight)?;
    if root.parent().is_none() || root.parent().is_some_and(|parent| parent == root) {
        return Err(TerminalFailure::ResetPreflight);
    }
    Ok(root)
}

fn capture_reset_root(root: &Path) -> Result<ResetRootExpectation, TerminalFailure> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_dir() => root_identity(root)
            .map(ResetRootExpectation::Present)
            .map_err(|_| TerminalFailure::ResetPreflight),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ResetRootExpectation::Absent),
        Ok(_) | Err(_) => Err(TerminalFailure::ResetPreflight),
    }
}

#[cfg(unix)]
fn root_identity(path: &Path) -> io::Result<ResetRootIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    Ok(ResetRootIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn root_identity(path: &Path) -> io::Result<ResetRootIdentity> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_dir() {
        return Err(io::Error::other("reset root is not a directory"));
    }

    let first = open_reset_root(path)?;
    let first_metadata = first.metadata()?;
    if !first_metadata.file_type().is_dir() {
        return Err(io::Error::other("reset root changed while opening"));
    }
    let identity = reset_root_identity_from_file(&first)?;

    let after = fs::symlink_metadata(path)?;
    if !after.file_type().is_dir() {
        return Err(io::Error::other("reset root changed while opening"));
    }
    let second = open_reset_root(path)?;
    if !second.metadata()?.file_type().is_dir()
        || reset_root_identity_from_file(&second)? != identity
    {
        return Err(io::Error::other("reset root changed while opening"));
    }
    Ok(identity)
}

#[cfg(windows)]
fn open_reset_root(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(windows)]
fn reset_root_identity_from_file(file: &fs::File) -> io::Result<ResetRootIdentity> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` owns a valid handle, and `info` is a correctly sized writable buffer.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ResetRootIdentity::Windows {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn root_identity(_path: &Path) -> io::Result<ResetRootIdentity> {
    Err(io::Error::other(
        "stable reset root identity is unavailable on this platform",
    ))
}

fn path_resolves_within(candidate: &Path, root: &Path) -> io::Result<bool> {
    let lexical_candidate = absolute_lexical(candidate)?;
    let lexical_root = absolute_lexical(root)?;
    if lexical_candidate.starts_with(&lexical_root) {
        return Ok(true);
    }

    match (fs::canonicalize(candidate), fs::canonicalize(root)) {
        (Ok(candidate), Ok(root)) => Ok(candidate.starts_with(root)),
        _ => Ok(false),
    }
}

fn absolute_lexical(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "path escapes its filesystem root",
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

async fn delete_reset_root_off_runtime(plan: TerminalResetPlan) -> TerminalResult {
    tauri::async_runtime::spawn_blocking(move || delete_reset_root(&plan))
        .await
        .map_err(|_| TerminalFailure::ResetDeletion)?
        .map_err(|_| TerminalFailure::ResetDeletion)
}

fn delete_reset_root(plan: &TerminalResetPlan) -> io::Result<()> {
    match plan.expected_root {
        ResetRootExpectation::Absent => match fs::symlink_metadata(&plan.config_root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => {
                return Err(io::Error::other(
                    "reset root appeared after absence was proven",
                ));
            }
            Err(error) => return Err(error),
        },
        ResetRootExpectation::Present(expected) => {
            let metadata = fs::symlink_metadata(&plan.config_root)?;
            if !metadata.file_type().is_dir() || root_identity(&plan.config_root)? != expected {
                return Err(io::Error::other(
                    "reset root identity changed after preflight",
                ));
            }
            fs::remove_dir_all(&plan.config_root)?;
        }
    }

    match fs::symlink_metadata(&plan.config_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(io::Error::other("reset root still exists after deletion")),
        Err(error) => Err(error),
    }
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
            let _ = app.emit(&event_name, public_vanilla_install_progress_json(&progress));
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
                    let _ = app.emit(&event_name, public_vanilla_install_progress_json(&progress));
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
            let _ = app.emit(&event_name, public_loader_install_progress_json(&progress));
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
                    let _ = app.emit(&event_name, public_loader_install_progress_json(&progress));
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
        let snapshot_status = snapshot_status(&snapshot);
        let _ = app.emit(
            &status_event_name,
            public_launch_status_json(&snapshot_status),
        );
        if is_terminal_state(snapshot.state) {
            return;
        }
        loop {
            match receiver.recv().await {
                Ok(LaunchEvent::Status(status)) => {
                    let terminal = matches!(status.state.as_str(), "failed" | "exited");
                    let _ = app.emit(&status_event_name, public_launch_status_json(&status));
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

#[cfg(test)]
mod tests {
    use super::{
        CLOSE_BUSY_MESSAGE, PNG_SIGNATURE, RESTART_BUSY_MESSAGE, SKIN_FILE_MAX_BYTES,
        TerminalFailure, build_reset_plan, close_readiness, delete_reset_root,
        read_skin_file_from_path, restart_readiness,
    };
    use axial_config::{AppConfig, AppPaths};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "axial-desktop-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        dir
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
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
    fn reset_plan_rejects_external_paths_merely_labeled_managed() {
        let root = test_dir("reset-external-managed");
        let paths = test_paths(&root);
        let external = root.join("external-library");
        fs::create_dir_all(&external).expect("external library");
        let config = AppConfig {
            library_dir: external.to_string_lossy().to_string(),
            library_mode: "managed".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(
            build_reset_plan(
                &paths,
                &config,
                &std::env::current_exe().expect("test executable"),
            ),
            Err(TerminalFailure::ResetPreflight)
        );
        assert!(external.exists());
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_plan_preserves_external_existing_library() {
        let root = test_dir("reset-external-existing");
        let paths = test_paths(&root);
        let external = root.join("external-library");
        fs::create_dir_all(&external).expect("external library");
        let config = AppConfig {
            library_dir: external.to_string_lossy().to_string(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        };

        assert!(
            build_reset_plan(
                &paths,
                &config,
                &std::env::current_exe().expect("test executable"),
            )
            .is_ok()
        );
        assert!(external.exists());
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_plan_rejects_existing_library_nested_in_config_root() {
        let root = test_dir("reset-nested-existing");
        let paths = test_paths(&root);
        let existing = paths.config_dir.join("user-library");
        fs::create_dir_all(&existing).expect("nested existing library");
        let config = AppConfig {
            library_dir: existing.to_string_lossy().to_string(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(
            build_reset_plan(
                &paths,
                &config,
                &std::env::current_exe().expect("test executable"),
            ),
            Err(TerminalFailure::ResetPreflight)
        );
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_plan_rejects_non_file_restart_target() {
        let root = test_dir("reset-restart-target");
        let paths = test_paths(&root);

        assert_eq!(
            build_reset_plan(&paths, &AppConfig::default(), &root),
            Err(TerminalFailure::ResetPreflight)
        );
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_plan_rejects_unknown_library_mode() {
        let root = test_dir("reset-unknown-library-mode");
        let paths = test_paths(&root);
        let config = AppConfig {
            library_mode: "legacy".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(
            build_reset_plan(
                &paths,
                &config,
                &std::env::current_exe().expect("test executable"),
            ),
            Err(TerminalFailure::ResetPreflight)
        );
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_deletion_removes_only_the_preflight_root_identity() {
        let root = test_dir("reset-delete");
        let paths = test_paths(&root);
        let config_root = paths.config_dir.clone();
        let external = root.join("external");
        fs::create_dir_all(config_root.join("instances")).expect("config root");
        fs::create_dir_all(&external).expect("external root");
        fs::write(config_root.join("config.json"), "state").expect("config file");
        let plan = build_reset_plan(
            &paths,
            &AppConfig::default(),
            &std::env::current_exe().expect("test executable"),
        )
        .expect("present reset plan");

        delete_reset_root(&plan).expect("first delete");

        assert!(!config_root.exists());
        assert!(external.exists());
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn absent_reset_root_remains_idempotently_absent() {
        let root = test_dir("reset-absent");
        let paths = test_paths(&root);
        let plan = build_reset_plan(
            &paths,
            &AppConfig::default(),
            &std::env::current_exe().expect("test executable"),
        )
        .expect("absent reset plan");

        delete_reset_root(&plan).expect("first absent proof");
        delete_reset_root(&plan).expect("second absent proof");

        assert!(!paths.config_dir.exists());
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_deletion_rejects_root_created_after_absent_preflight() {
        let root = test_dir("reset-absent-then-created");
        let paths = test_paths(&root);
        let plan = build_reset_plan(
            &paths,
            &AppConfig::default(),
            &std::env::current_exe().expect("test executable"),
        )
        .expect("absent reset plan");
        fs::create_dir_all(&paths.config_dir).expect("late replacement root");
        fs::write(paths.config_dir.join("preserved"), "replacement").expect("replacement marker");

        assert!(delete_reset_root(&plan).is_err());
        assert_eq!(
            fs::read_to_string(paths.config_dir.join("preserved")).expect("preserved replacement"),
            "replacement"
        );
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[test]
    fn reset_deletion_rejects_renamed_and_replaced_root() {
        let root = test_dir("reset-replaced");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("original config root");
        fs::write(paths.config_dir.join("original"), "original").expect("original marker");
        let plan = build_reset_plan(
            &paths,
            &AppConfig::default(),
            &std::env::current_exe().expect("test executable"),
        )
        .expect("present reset plan");
        let parked = root.join("parked-config");
        fs::rename(&paths.config_dir, &parked).expect("park original root");
        fs::create_dir_all(&paths.config_dir).expect("replacement config root");
        fs::write(paths.config_dir.join("replacement"), "replacement").expect("replacement marker");

        assert!(delete_reset_root(&plan).is_err());
        assert!(parked.join("original").exists());
        assert!(paths.config_dir.join("replacement").exists());
        fs::remove_dir_all(root).expect("cleanup test dir");
    }

    #[cfg(unix)]
    #[test]
    fn reset_plan_and_deletion_reject_root_symlink_without_traversing_target() {
        use std::os::unix::fs::symlink;

        let root = test_dir("reset-symlink");
        let target = root.join("target");
        let link = root.join("config");
        fs::create_dir_all(&target).expect("symlink target");
        fs::write(target.join("preserved"), "user data").expect("target data");
        symlink(&target, &link).expect("config symlink");
        let paths = test_paths(&root);
        assert_eq!(
            build_reset_plan(
                &paths,
                &AppConfig::default(),
                &std::env::current_exe().expect("test executable"),
            ),
            Err(TerminalFailure::ResetPreflight)
        );

        assert!(fs::symlink_metadata(link).is_ok());
        assert_eq!(
            fs::read_to_string(target.join("preserved")).expect("preserved target"),
            "user data"
        );
        fs::remove_dir_all(root).expect("cleanup test dir");
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
