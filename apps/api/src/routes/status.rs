use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: &'static str,
    warnings: Vec<String>,
    library_dir: String,
    library_mode: String,
    setup_required: bool,
    app_name: String,
    version: String,
    dev_mode: bool,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/status", get(handle_status))
}

async fn handle_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let config = state.config().current();
    let library_dir = state.library_dir().unwrap_or_default();

    Json(StatusResponse {
        status: "ok",
        warnings: state.startup_warnings(),
        setup_required: library_dir.is_empty(),
        library_dir,
        library_mode: config.library_mode,
        app_name: state.app_name().to_string(),
        version: state.version().to_string(),
        dev_mode: cfg!(debug_assertions),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[tokio::test]
    async fn status_includes_startup_warnings_and_remains_ok() {
        let root = test_root("status-startup-warnings");
        let paths = test_paths(&root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        let state = AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: vec!["startup warning".to_string()],
            frontend_dir: root.join("frontend"),
        });

        let Json(response) = handle_status(State(state)).await;

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings, vec!["startup warning".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn status_includes_instance_registry_startup_warning_and_remains_ok() {
        let root = test_root("status-instance-startup-warning");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        fs::write(&paths.instances_file, "{not valid json").expect("write malformed registry");

        let config_startup =
            ConfigStore::load_for_startup(paths.clone()).expect("load config for startup");
        let instance_startup = InstanceStore::load_for_startup(paths.clone());
        let mut startup_warnings = config_startup.warnings;
        startup_warnings.extend(instance_startup.warnings);
        let state = AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config: Arc::new(config_startup.store),
            instances: Arc::new(instance_startup.store),
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings,
            frontend_dir: root.join("frontend"),
        });

        let Json(response) = handle_status(State(state)).await;

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(
            response.warnings[0],
            "Croopor could not load the instance list, so it started with an empty list. Check app data permissions or restore the instance registry."
        );
        assert!(!response.warnings[0].contains(&root.to_string_lossy().to_string()));
        assert!(!response.warnings[0].contains("expected"));
        assert!(!response.warnings[0].contains("line"));

        let _ = fs::remove_dir_all(root);
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "croopor-api-status-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
