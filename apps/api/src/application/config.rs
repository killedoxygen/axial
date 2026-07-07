//! Application-owned settings workflow.

use crate::{
    application,
    observability::telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    },
    state::AppState,
};
use axum::{Json, http::StatusCode};
use croopor_config::{AppConfig, ConfigStoreError};
use serde::Deserialize;

const CONFIG_SAVE_ERROR_MESSAGE: &str =
    "Could not save settings. Check app data permissions and try again.";

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Default, Deserialize)]
pub struct ConfigPatch {
    username: Option<String>,
    launch_auth_mode: Option<String>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
    java_path_override: Option<String>,
    window_width: Option<i32>,
    window_height: Option<i32>,
    onboarding_done: Option<bool>,
    jvm_preset: Option<String>,
    performance_mode: Option<String>,
    guardian_mode: Option<String>,
    theme: Option<String>,
    custom_hue: Option<i32>,
    custom_vibrancy: Option<i32>,
    lightness: Option<i32>,
    telemetry_enabled: Option<bool>,
    discord_rpc_enabled: Option<bool>,
    discord_rpc_onboarding_seen: Option<bool>,
    music_enabled: Option<bool>,
    music_volume: Option<i32>,
    music_track: Option<i32>,
    library_dir: Option<String>,
    library_mode: Option<String>,
}

pub fn current_config(state: &AppState) -> AppConfig {
    state.config().current()
}

pub fn update_config(state: &AppState, patch: ConfigPatch) -> Result<AppConfig, ApiError> {
    let mut next = state.config().current();
    let previous_library_dir = next.library_dir.clone();
    let sync_offline_username = patch.username.is_some();
    if let Some(username) = patch.username {
        next.username = username;
    }
    if let Some(launch_auth_mode) = patch.launch_auth_mode {
        next.launch_auth_mode = launch_auth_mode;
    }
    if let Some(max_memory_mb) = patch.max_memory_mb.filter(|value| *value > 0) {
        next.max_memory_mb = max_memory_mb;
    }
    if let Some(min_memory_mb) = patch.min_memory_mb.filter(|value| *value > 0) {
        next.min_memory_mb = min_memory_mb;
    }
    if let Some(java_path_override) = patch.java_path_override {
        next.java_path_override = java_path_override;
    }
    if let Some(window_width) = patch.window_width {
        next.window_width = window_width;
    }
    if let Some(window_height) = patch.window_height {
        next.window_height = window_height;
    }
    if let Some(onboarding_done) = patch.onboarding_done {
        next.onboarding_done = onboarding_done;
    }
    if let Some(jvm_preset) = patch.jvm_preset {
        next.jvm_preset = jvm_preset;
    }
    if let Some(performance_mode) = patch.performance_mode {
        next.performance_mode = performance_mode;
    }
    if let Some(guardian_mode) = patch.guardian_mode {
        next.guardian_mode = guardian_mode;
    }
    if let Some(theme) = patch.theme {
        next.theme = theme;
    }
    if let Some(custom_hue) = patch.custom_hue {
        next.custom_hue = Some(custom_hue);
    }
    if let Some(custom_vibrancy) = patch.custom_vibrancy {
        next.custom_vibrancy = Some(custom_vibrancy);
    }
    if let Some(lightness) = patch.lightness {
        next.lightness = Some(lightness);
    }
    if let Some(telemetry_enabled) = patch.telemetry_enabled {
        next.telemetry_enabled = telemetry_enabled;
    }
    if let Some(discord_rpc_enabled) = patch.discord_rpc_enabled {
        next.discord_rpc_enabled = discord_rpc_enabled;
    }
    if let Some(discord_rpc_onboarding_seen) = patch.discord_rpc_onboarding_seen {
        next.discord_rpc_onboarding_seen = discord_rpc_onboarding_seen;
    }
    if let Some(music_enabled) = patch.music_enabled {
        next.music_enabled = Some(music_enabled);
    }
    if let Some(music_volume) = patch.music_volume {
        next.music_volume = Some(music_volume);
    }
    if let Some(music_track) = patch.music_track {
        next.music_track = music_track.max(0);
    }
    if let Some(library_dir) = patch.library_dir {
        next.library_dir = library_dir;
    }
    if let Some(library_mode) = patch.library_mode {
        next.library_mode = library_mode;
    }

    let telemetry_enabled_after_patch = next.telemetry_enabled;

    match state.update_config(next) {
        Ok(config) => {
            if config.library_dir != previous_library_dir {
                application::instances::invalidate_create_view_cache();
            }
            if sync_offline_username {
                application::sync_active_offline_account_from_username(state, &config.username)
                    .map_err(config_account_sync_error_response)?;
            }
            Ok(config)
        }
        Err(error) => {
            emit_config_save_failed(state, &error, telemetry_enabled_after_patch);
            Err(config_update_error_response(error))
        }
    }
}

fn emit_config_save_failed(
    state: &AppState,
    error: &ConfigStoreError,
    telemetry_enabled_after_patch: bool,
) {
    if !telemetry_enabled_after_patch || matches!(error, ConfigStoreError::Validation(_)) {
        return;
    }
    state.telemetry().emit(TelemetryEvent::error_captured(
        TelemetryErrorKind::ConfigSaveFailed,
        TelemetryErrorArea::Config,
        TelemetryErrorLevel::Error,
        CONFIG_SAVE_ERROR_MESSAGE,
    ));
}

fn config_update_error_response(error: ConfigStoreError) -> ApiError {
    match error {
        ConfigStoreError::Validation(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": CONFIG_SAVE_ERROR_MESSAGE })),
        ),
    }
}

fn config_account_sync_error_response(error: std::io::Error) -> ApiError {
    let status = if error.kind() == std::io::ErrorKind::InvalidInput {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(serde_json::json!({ "error": CONFIG_SAVE_ERROR_MESSAGE })),
    )
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_SAVE_ERROR_MESSAGE, ConfigPatch, config_update_error_response};
    use crate::{
        observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub},
        state::{AppState, AppStateInit, InstallStore, SessionStore},
    };
    use axum::Json;
    use croopor_config::{
        AppConfig, AppConfigValidationError, AppPaths, ConfigStore, ConfigStoreError, InstanceStore,
    };
    use croopor_performance::PerformanceManager;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    const TEST_TELEMETRY_KEY: &str = "phc_test";
    const TEST_TELEMETRY_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";

    #[test]
    fn config_patch_accepts_telemetry_enabled() {
        let patch = serde_json::from_value::<ConfigPatch>(serde_json::json!({
            "telemetry_enabled": true
        }))
        .expect("telemetry consent patch should deserialize");

        assert_eq!(patch.telemetry_enabled, Some(true));
    }

    #[test]
    fn config_patch_accepts_discord_rpc_flags() {
        let patch = serde_json::from_value::<ConfigPatch>(serde_json::json!({
            "discord_rpc_enabled": false,
            "discord_rpc_onboarding_seen": true
        }))
        .expect("discord rpc patch should deserialize");

        assert_eq!(patch.discord_rpc_enabled, Some(false));
        assert_eq!(patch.discord_rpc_onboarding_seen, Some(true));
    }

    #[test]
    fn config_update_validation_error_keeps_details() {
        let (status, Json(body)) = config_update_error_response(ConfigStoreError::Validation(
            AppConfigValidationError::InvalidUsername("Letters, numbers, and underscores only."),
        ));

        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            serde_json::json!({
                "error": "invalid username: Letters, numbers, and underscores only."
            })
        );
    }

    #[test]
    fn config_update_non_validation_error_hides_local_paths() {
        let paths = [
            "/Users/alice/Library/Application Support/Croopor/config.json",
            r"C:\Users\Alice\AppData\Roaming\Croopor\config.json",
        ];

        for path in paths {
            let (status, Json(body)) =
                config_update_error_response(ConfigStoreError::Read(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("permission denied writing {path}"),
                )));
            let message = body
                .get("error")
                .and_then(|value| value.as_str())
                .expect("error response should include a string message");

            assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(message, CONFIG_SAVE_ERROR_MESSAGE);
            assert!(!message.contains(path));
        }
    }

    #[tokio::test]
    async fn config_username_update_renames_active_offline_account() {
        let fixture = TestFixture::new("username-offline-sync");
        fixture
            .state
            .accounts()
            .create_offline_account("OldName")
            .expect("create offline account");

        let config = super::update_config(
            &fixture.state,
            ConfigPatch {
                username: Some("NewName".to_string()),
                ..ConfigPatch::default()
            },
        )
        .expect("update config");

        assert_eq!(config.username, "NewName");
        let active = fixture
            .state
            .accounts()
            .active_account()
            .expect("active account")
            .expect("active account");
        assert_eq!(active.display_name, "NewName");
        assert_eq!(fixture.state.config().current().username, "NewName");
    }

    #[tokio::test]
    async fn config_update_notifies_config_subscribers() {
        let fixture = TestFixture::new("config-change-notify");
        let mut changes = fixture.state.subscribe_config_changes();

        let config = super::update_config(
            &fixture.state,
            ConfigPatch {
                discord_rpc_enabled: Some(false),
                ..ConfigPatch::default()
            },
        )
        .expect("update config");

        assert!(!config.discord_rpc_enabled);
        tokio::time::timeout(std::time::Duration::from_secs(1), changes.recv())
            .await
            .expect("config change notification should arrive")
            .expect("config change sender should remain open");
    }

    #[test]
    fn config_save_failure_emits_when_requested_config_keeps_telemetry_enabled() {
        let fixture = TestFixture::new("config-save-failure-emit");
        fixture.seed_config(AppConfig {
            telemetry_enabled: true,
            telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
            ..AppConfig::default()
        });
        fixture.block_config_dir();

        let result = super::update_config(
            &fixture.state,
            ConfigPatch {
                discord_rpc_enabled: Some(false),
                ..ConfigPatch::default()
            },
        );

        assert!(result.is_err());
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 1);
    }

    #[test]
    fn config_save_failure_does_not_emit_when_request_disables_telemetry() {
        let fixture = TestFixture::new("config-save-failure-disable-telemetry");
        fixture.seed_config(AppConfig {
            telemetry_enabled: true,
            telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
            ..AppConfig::default()
        });
        fixture.block_config_dir();

        let result = super::update_config(
            &fixture.state,
            ConfigPatch {
                telemetry_enabled: Some(false),
                ..ConfigPatch::default()
            },
        );

        assert!(result.is_err());
        assert!(fixture.state.config().current().telemetry_enabled);
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 0);
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig::default())
                .expect("set config");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let telemetry = Arc::new(TelemetryHub::new(
                config.clone(),
                Some(TEST_TELEMETRY_KEY.to_string()),
                DEFAULT_POSTHOG_HOST.to_string(),
            ));
            let state = AppState::new_with_telemetry(
                AppStateInit {
                    app_name: "Croopor".to_string(),
                    version: "test".to_string(),
                    config,
                    instances,
                    installs: Arc::new(InstallStore::new()),
                    sessions: Arc::new(SessionStore::new()),
                    performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                    startup_warnings: Vec::new(),
                    frontend_dir: root.join("frontend"),
                },
                telemetry,
            );

            Self { state, root }
        }

        fn seed_config(&self, config: AppConfig) {
            self.state
                .config()
                .replace_in_memory(config)
                .expect("seed config");
        }

        fn block_config_dir(&self) {
            fs::create_dir_all(&self.root).expect("create test root");
            fs::write(self.root.join("config"), "not a directory")
                .expect("block config directory with file");
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
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

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "croopor-config-route-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
