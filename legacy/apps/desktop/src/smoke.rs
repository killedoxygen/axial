use std::io;
use std::path::PathBuf;
use tauri::utils::config::WindowConfig;

#[cfg(any(debug_assertions, test))]
const WEBVIEW_DATA_DIR_ENV: &str = "AXIAL_DESKTOP_WEBVIEW_DATA_DIR";
const MAIN_WINDOW_LABEL: &str = "main";
#[cfg(any(debug_assertions, test))]
const EMPTY_WEBVIEW_DATA_DIR_ERROR: &str = "AXIAL_DESKTOP_WEBVIEW_DATA_DIR must not be empty";
#[cfg(any(debug_assertions, test))]
const RELATIVE_WEBVIEW_DATA_DIR_ERROR: &str =
    "AXIAL_DESKTOP_WEBVIEW_DATA_DIR must be an absolute path";
const MISSING_MAIN_WINDOW_ERROR: &str = "desktop configuration is missing the main window";
const DUPLICATE_MAIN_WINDOW_ERROR: &str = "desktop configuration contains multiple main windows";

#[derive(Clone, Debug)]
pub(super) struct IsolatedMainWindow {
    pub(super) config: WindowConfig,
    pub(super) data_directory: PathBuf,
}

pub(super) fn webview_data_directory() -> io::Result<Option<PathBuf>> {
    #[cfg(any(debug_assertions, test))]
    {
        parse_webview_data_directory(std::env::var_os(WEBVIEW_DATA_DIR_ENV))
    }

    #[cfg(not(any(debug_assertions, test)))]
    {
        Ok(None)
    }
}

pub(super) fn isolate_main_window(
    windows: &mut [WindowConfig],
    data_directory: Option<PathBuf>,
) -> io::Result<Option<IsolatedMainWindow>> {
    let Some(data_directory) = data_directory else {
        return Ok(None);
    };
    let mut matches = windows
        .iter_mut()
        .filter(|window| window.label == MAIN_WINDOW_LABEL);
    let main = matches
        .next()
        .ok_or_else(|| invalid_input(MISSING_MAIN_WINDOW_ERROR))?;
    if matches.next().is_some() {
        return Err(invalid_input(DUPLICATE_MAIN_WINDOW_ERROR));
    }

    let config = main.clone();
    main.create = false;
    Ok(Some(IsolatedMainWindow {
        config,
        data_directory,
    }))
}

#[cfg(any(debug_assertions, test))]
fn parse_webview_data_directory(value: Option<std::ffi::OsString>) -> io::Result<Option<PathBuf>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(invalid_input(EMPTY_WEBVIEW_DATA_DIR_ERROR));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(invalid_input(RELATIVE_WEBVIEW_DATA_DIR_ERROR));
    }
    Ok(Some(path))
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn webview_data_directory_is_optional() {
        assert_eq!(parse_webview_data_directory(None).unwrap(), None);
    }

    #[test]
    fn webview_data_directory_rejects_empty_and_relative_values_without_echoing_them() {
        for (value, expected) in [
            (OsString::new(), EMPTY_WEBVIEW_DATA_DIR_ERROR),
            (
                OsString::from("private-relative-profile"),
                RELATIVE_WEBVIEW_DATA_DIR_ERROR,
            ),
        ] {
            let error = parse_webview_data_directory(Some(value)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(error.to_string(), expected);
            assert!(error.to_string().len() < 96);
            assert!(!error.to_string().contains("private-relative-profile"));
        }
    }

    #[test]
    fn webview_data_directory_accepts_an_absolute_path_without_creating_it() {
        let path = std::env::temp_dir().join("axial-desktop-webview-isolation-test");
        let parsed = parse_webview_data_directory(Some(path.clone().into_os_string())).unwrap();

        assert_eq!(parsed, Some(path));
    }

    #[test]
    fn isolation_preserves_the_complete_main_window_builder_config() {
        let mut main = WindowConfig {
            label: MAIN_WINDOW_LABEL.to_string(),
            title: "Axial smoke fixture".to_string(),
            width: 1_100.0,
            height: 720.0,
            min_width: Some(960.0),
            min_height: Some(640.0),
            decorations: false,
            transparent: false,
            shadow: true,
            ..WindowConfig::default()
        };
        main.url = tauri::utils::config::WebviewUrl::App("index.html".into());
        let original = main.clone();
        let secondary = WindowConfig {
            label: "secondary".to_string(),
            title: "Secondary".to_string(),
            ..WindowConfig::default()
        };
        let mut windows = vec![main, secondary.clone()];
        let data_directory = std::env::temp_dir().join("axial-desktop-main-webview");

        let isolated = isolate_main_window(&mut windows, Some(data_directory.clone()))
            .unwrap()
            .expect("isolation config");

        assert_eq!(isolated.config, original);
        assert_eq!(isolated.data_directory, data_directory);
        assert!(!windows[0].create);
        windows[0].create = true;
        assert_eq!(windows[0], original);
        assert_eq!(windows[1], secondary);
    }

    #[test]
    fn isolation_is_a_noop_without_the_debug_hook() {
        let original = WindowConfig::default();
        let mut windows = vec![original.clone()];

        assert!(isolate_main_window(&mut windows, None).unwrap().is_none());
        assert_eq!(windows, [original]);
    }

    #[test]
    fn isolation_rejects_missing_or_duplicate_main_configs() {
        let data_directory = std::env::temp_dir().join("axial-desktop-main-webview");
        let mut missing = vec![WindowConfig {
            label: "secondary".to_string(),
            ..WindowConfig::default()
        }];
        let missing_error =
            isolate_main_window(&mut missing, Some(data_directory.clone())).unwrap_err();
        assert_eq!(missing_error.to_string(), MISSING_MAIN_WINDOW_ERROR);

        let mut duplicate = vec![WindowConfig::default(), WindowConfig::default()];
        let duplicate_error =
            isolate_main_window(&mut duplicate, Some(data_directory)).unwrap_err();
        assert_eq!(duplicate_error.to_string(), DUPLICATE_MAIN_WINDOW_ERROR);
    }
}
