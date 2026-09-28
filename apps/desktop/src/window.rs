use crate::bootstrap::DesktopBootstrap;
use serde::Serialize;
use std::path::PathBuf;
use tauri::webview::{Color, NewWindowResponse};
use tauri::{AppHandle, State, WebviewWindow, WebviewWindowBuilder};

pub const MAIN_WINDOW: &str = "main";

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct NativeDesktopChrome {
    platform: &'static str,
    chrome_mode: &'static str,
}

fn chrome_mode(platform: &str) -> &'static str {
    match platform {
        "macos" => "mac-overlay",
        "windows" | "linux" => "custom-frameless",
        _ => "native-decorated",
    }
}

pub fn require_main_window(
    window: &WebviewWindow,
    bootstrap: &DesktopBootstrap,
) -> Result<(), String> {
    if window.label() != MAIN_WINDOW
        || !window
            .url()
            .is_ok_and(|url| bootstrap.allows_main_url(&url))
    {
        return Err("This native action is available only in the main Axial window.".into());
    }
    Ok(())
}

pub fn build_main_window(
    app: &AppHandle,
    config: &tauri::utils::config::WindowConfig,
    webview_directory: PathBuf,
    bootstrap: DesktopBootstrap,
) -> tauri::Result<WebviewWindow> {
    let builder = WebviewWindowBuilder::from_config(app, config)?
        .data_directory(webview_directory)
        // WKWebView ignores data_directory; durable preferences live in profile metadata.
        .incognito(config.incognito || cfg!(target_os = "macos"))
        .decorations(!cfg!(any(target_os = "windows", target_os = "linux")))
        .on_navigation(move |url| bootstrap.allows_main_url(url))
        .on_new_window(|_, _| NewWindowResponse::Deny);
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(16.0, 19.0));
    builder.build()
}

#[tauri::command]
pub fn desktop_chrome(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
) -> Result<NativeDesktopChrome, String> {
    require_main_window(&window, &bootstrap)?;
    Ok(NativeDesktopChrome {
        platform: std::env::consts::OS,
        chrome_mode: chrome_mode(std::env::consts::OS),
    })
}

#[tauri::command]
pub fn window_minimize(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
) -> Result<(), String> {
    require_main_window(&window, &bootstrap)?;
    window
        .minimize()
        .map_err(|_| "Could not minimize the window.".into())
}

#[tauri::command]
pub fn window_toggle_maximize(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
) -> Result<bool, String> {
    require_main_window(&window, &bootstrap)?;
    let maximized = window
        .is_maximized()
        .map_err(|_| "Could not read the window state.")?;
    if maximized {
        window.unmaximize()
    } else {
        window.maximize()
    }
    .map_err(|_| "Could not change the window state.")?;
    Ok(!maximized)
}

#[tauri::command]
pub fn window_is_maximized(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
) -> Result<bool, String> {
    require_main_window(&window, &bootstrap)?;
    window
        .is_maximized()
        .map_err(|_| "Could not read the window state.".into())
}

#[tauri::command]
pub fn window_start_dragging(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
) -> Result<(), String> {
    require_main_window(&window, &bootstrap)?;
    window
        .start_dragging()
        .map_err(|_| "Could not drag the window.".into())
}

#[tauri::command]
pub fn window_set_resize_background(
    window: WebviewWindow,
    bootstrap: State<'_, DesktopBootstrap>,
    dark: bool,
) -> Result<(), String> {
    require_main_window(&window, &bootstrap)?;
    let color = if dark {
        Color(16, 13, 10, 255)
    } else {
        Color(244, 241, 237, 255)
    };
    window
        .set_background_color(Some(color))
        .map_err(|_| "Could not update the window background.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_matches_each_platform_window() {
        assert_eq!(chrome_mode("macos"), "mac-overlay");
        assert_eq!(chrome_mode("windows"), "custom-frameless");
        assert_eq!(chrome_mode("linux"), "custom-frameless");
    }
}
