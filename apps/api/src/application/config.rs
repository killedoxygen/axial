//! Application-owned settings workflow.

use crate::{
    application,
    observability::telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    },
    state::AppState,
};
use axial_config::{AppConfig, ConfigStoreError};
use axum::{Json, http::StatusCode};
use serde::Deserialize;

const CONFIG_SAVE_ERROR_MESSAGE: &str =
    "Could not save settings. Check app data permissions and try again.";

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    guardian_idle_integrity_enabled: Option<bool>,
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
}

pub fn current_config(state: &AppState) -> AppConfig {
    state.config().current()
}

pub async fn update_config(state: &AppState, patch: ConfigPatch) -> Result<AppConfig, ApiError> {
    let sync_offline_username = patch.username.is_some();
    let emit_save_failure = patch.telemetry_enabled != Some(false);

    match state
        .mutate_config(move |latest| -> Result<(), ConfigStoreError> {
            if let Some(username) = patch.username {
                latest.username = username;
            }
            if let Some(launch_auth_mode) = patch.launch_auth_mode {
                latest.launch_auth_mode = launch_auth_mode;
            }
            if let Some(max_memory_mb) = patch.max_memory_mb.filter(|value| *value > 0) {
                latest.max_memory_mb = max_memory_mb;
            }
            if let Some(min_memory_mb) = patch.min_memory_mb.filter(|value| *value > 0) {
                latest.min_memory_mb = min_memory_mb;
            }
            if let Some(java_path_override) = patch.java_path_override {
                latest.java_path_override = java_path_override;
            }
            if let Some(window_width) = patch.window_width {
                latest.window_width = window_width;
            }
            if let Some(window_height) = patch.window_height {
                latest.window_height = window_height;
            }
            if let Some(onboarding_done) = patch.onboarding_done {
                latest.onboarding_done = onboarding_done;
            }
            if let Some(jvm_preset) = patch.jvm_preset {
                latest.jvm_preset = jvm_preset;
            }
            if let Some(performance_mode) = patch.performance_mode {
                latest.performance_mode = performance_mode;
            }
            if let Some(guardian_mode) = patch.guardian_mode {
                latest.guardian_mode = guardian_mode;
            }
            if let Some(guardian_idle_integrity_enabled) = patch.guardian_idle_integrity_enabled {
                latest.guardian_idle_integrity_enabled = guardian_idle_integrity_enabled;
            }
            if let Some(theme) = patch.theme {
                latest.theme = theme;
            }
            if let Some(custom_hue) = patch.custom_hue {
                latest.custom_hue = Some(custom_hue);
            }
            if let Some(custom_vibrancy) = patch.custom_vibrancy {
                latest.custom_vibrancy = Some(custom_vibrancy);
            }
            if let Some(lightness) = patch.lightness {
                latest.lightness = Some(lightness);
            }
            if let Some(telemetry_enabled) = patch.telemetry_enabled {
                latest.telemetry_enabled = telemetry_enabled;
            }
            if let Some(discord_rpc_enabled) = patch.discord_rpc_enabled {
                latest.discord_rpc_enabled = discord_rpc_enabled;
            }
            if let Some(discord_rpc_onboarding_seen) = patch.discord_rpc_onboarding_seen {
                latest.discord_rpc_onboarding_seen = discord_rpc_onboarding_seen;
            }
            if let Some(music_enabled) = patch.music_enabled {
                latest.music_enabled = Some(music_enabled);
            }
            if let Some(music_volume) = patch.music_volume {
                latest.music_volume = Some(music_volume);
            }
            if let Some(music_track) = patch.music_track {
                latest.music_track = music_track.max(0);
            }
            Ok(())
        })
        .await
    {
        Ok(config) => {
            if sync_offline_username {
                application::sync_active_offline_account_from_username(state, &config.username)
                    .await
                    .map_err(config_account_sync_error_response)?;
            }
            Ok(config)
        }
        Err(error) => {
            emit_config_save_failed(state, &error, emit_save_failure);
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
        observability::telemetry::{
            DEFAULT_POSTHOG_HOST, TelemetryEvent, TelemetryHub, TelemetryLaunchOutcome,
        },
        state::{AppState, AppStateInit, InstallStore, SessionStore},
    };
    use axial_config::{
        AppConfig, AppConfigValidationError, AppPaths, ConfigStore, ConfigStoreError,
        InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_performance::PerformanceManager;
    use axum::Json;
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
    fn config_patch_accepts_guardian_idle_integrity_setting() {
        let patch = serde_json::from_value::<ConfigPatch>(serde_json::json!({
            "guardian_idle_integrity_enabled": false
        }))
        .expect("guardian idle integrity patch should deserialize");

        assert_eq!(patch.guardian_idle_integrity_enabled, Some(false));
    }

    #[test]
    fn config_patch_rejects_library_ownership_fields() {
        for field in ["library_dir", "library_mode"] {
            let error = serde_json::from_value::<ConfigPatch>(serde_json::json!({
                (field): "caller-controlled"
            }))
            .expect_err("library ownership fields must not cross the generic config route");

            assert!(error.to_string().contains("unknown field"));
        }
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
            "/Users/alice/Library/Application Support/Axial/config.json",
            r"C:\Users\Alice\AppData\Roaming\Axial\config.json",
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
            .await
            .expect("create offline account");

        let config = super::update_config(
            &fixture.state,
            ConfigPatch {
                username: Some("NewName".to_string()),
                ..ConfigPatch::default()
            },
        )
        .await
        .expect("update config");

        assert_eq!(config.username, "NewName");
        let active = fixture
            .state
            .accounts()
            .active_account()
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
        .await
        .expect("update config");

        assert!(!config.discord_rpc_enabled);
        tokio::time::timeout(std::time::Duration::from_secs(1), changes.recv())
            .await
            .expect("config change notification should arrive")
            .expect("config change sender should remain open");
    }

    #[tokio::test]
    async fn config_update_persists_guardian_idle_integrity_setting() {
        let fixture = TestFixture::new("guardian-idle-integrity-setting");

        let disabled = super::update_config(
            &fixture.state,
            ConfigPatch {
                guardian_idle_integrity_enabled: Some(false),
                ..ConfigPatch::default()
            },
        )
        .await
        .expect("disable guardian idle integrity");

        assert!(!disabled.guardian_idle_integrity_enabled);
        assert!(
            !fixture
                .state
                .config()
                .current()
                .guardian_idle_integrity_enabled
        );

        let enabled = super::update_config(
            &fixture.state,
            ConfigPatch {
                guardian_idle_integrity_enabled: Some(true),
                ..ConfigPatch::default()
            },
        )
        .await
        .expect("enable guardian idle integrity");

        assert!(enabled.guardian_idle_integrity_enabled);
        assert!(
            fixture
                .state
                .config()
                .current()
                .guardian_idle_integrity_enabled
        );
    }

    #[tokio::test]
    async fn config_save_failure_emits_when_requested_config_keeps_telemetry_enabled() {
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
        )
        .await;

        assert!(result.is_err());
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 1);
    }

    #[tokio::test]
    async fn config_save_failure_does_not_emit_when_request_disables_telemetry() {
        let fixture = TestFixture::new("config-save-failure-disable-telemetry");
        fixture.seed_config(AppConfig {
            telemetry_enabled: true,
            telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
            ..AppConfig::default()
        });
        fixture
            .state
            .telemetry()
            .emit(TelemetryEvent::launch_completed(
                TelemetryLaunchOutcome::Success,
            ));
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 1);
        let mut changes = fixture.state.subscribe_config_changes();
        fixture.block_config_dir();

        let result = super::update_config(
            &fixture.state,
            ConfigPatch {
                telemetry_enabled: Some(false),
                ..ConfigPatch::default()
            },
        )
        .await;

        assert!(result.is_err());
        assert!(fixture.state.config().current().telemetry_enabled);
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 1);
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        fixture.unblock_config_dir();
        super::update_config(
            &fixture.state,
            ConfigPatch {
                telemetry_enabled: Some(false),
                ..ConfigPatch::default()
            },
        )
        .await
        .expect("retry retained telemetry disable");
        assert!(!fixture.state.config().current().telemetry_enabled);
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 0);
        changes
            .recv()
            .await
            .expect("durable retry publishes config change");
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), AppConfig::default()).expect("set config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("load instances"),
            );
            let telemetry = Arc::new(TelemetryHub::new(
                config.clone(),
                Some(TEST_TELEMETRY_KEY.to_string()),
                DEFAULT_POSTHOG_HOST.to_string(),
            ));
            let state = AppState::new_with_telemetry(
                AppStateInit {
                    app_name: "Axial".to_string(),
                    version: "test".to_string(),
                    config,
                    instances,
                    installs: Arc::new(InstallStore::new()),
                    sessions: Arc::new(SessionStore::new()),
                    performance: Arc::new(
                        PerformanceManager::load_for_startup(&paths.config_dir)
                            .expect("performance manager"),
                    ),
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
                .replace_for_test(config)
                .expect("seed config");
        }

        fn block_config_dir(&self) {
            fs::create_dir_all(&self.root).expect("create test root");
            fs::write(self.root.join("config"), "not a directory")
                .expect("block config directory with file");
        }

        fn unblock_config_dir(&self) {
            fs::remove_file(self.root.join("config")).expect("remove config directory blocker");
            fs::create_dir_all(self.root.join("config")).expect("restore config directory");
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
            "axial-config-route-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
