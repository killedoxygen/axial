//! Application-owned settings workflow.

use crate::{
    application,
    observability::telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    },
    state::AppState,
};
use axial_config::{
    AppConfig, ConfigGuardianMode, ConfigJvmPreset, ConfigLaunchAuthMode, ConfigPerformanceMode,
    ConfigStoreError, ConfigTheme,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

const CONFIG_SAVE_ERROR_MESSAGE: &str =
    "Could not save settings. Check app data permissions and try again.";

pub(super) type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigView {
    pub revision: u64,
    pub username: String,
    pub launch_auth_mode: ConfigLaunchAuthMode,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub java_path_override: String,
    pub window_width: i32,
    pub window_height: i32,
    pub onboarding_done: bool,
    pub jvm_preset: ConfigJvmPreset,
    pub performance_mode: ConfigPerformanceMode,
    pub guardian_mode: ConfigGuardianMode,
    pub guardian_idle_integrity_enabled: bool,
    pub theme: ConfigTheme,
    pub custom_hue: Option<i32>,
    pub custom_vibrancy: Option<i32>,
    pub lightness: Option<i32>,
    pub telemetry_enabled: bool,
    pub discord_rpc_enabled: bool,
    pub discord_rpc_onboarding_seen: bool,
    pub music_enabled: Option<bool>,
    pub music_volume: Option<i32>,
    pub music_track: i32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    username: Option<String>,
    launch_auth_mode: Option<ConfigLaunchAuthMode>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
    java_path_override: Option<String>,
    window_width: Option<i32>,
    window_height: Option<i32>,
    onboarding_done: Option<bool>,
    jvm_preset: Option<ConfigJvmPreset>,
    performance_mode: Option<ConfigPerformanceMode>,
    guardian_mode: Option<ConfigGuardianMode>,
    guardian_idle_integrity_enabled: Option<bool>,
    theme: Option<ConfigTheme>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfigFailureTelemetry {
    RespectCommittedConsent,
    SuppressForConsentDisable,
}

pub fn current_config(state: &AppState) -> ConfigView {
    ConfigView::from_snapshot(state.config().current_with_revision())
}

pub async fn update_config(state: &AppState, patch: ConfigPatch) -> Result<ConfigView, ApiError> {
    let sync_offline_username = patch.username.is_some();
    let failure_telemetry = if patch.telemetry_enabled == Some(false) {
        ConfigFailureTelemetry::SuppressForConsentDisable
    } else {
        ConfigFailureTelemetry::RespectCommittedConsent
    };

    let config = persist_config_mutation(state, failure_telemetry, move |latest| {
        if let Some(username) = patch.username {
            latest.username = username;
        }
        if let Some(launch_auth_mode) = patch.launch_auth_mode {
            latest.launch_auth_mode = launch_auth_mode.as_str().to_string();
        }
        if let Some(max_memory_mb) = patch.max_memory_mb {
            latest.max_memory_mb = max_memory_mb;
        }
        if let Some(min_memory_mb) = patch.min_memory_mb {
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
            latest.jvm_preset = jvm_preset.as_str().to_string();
        }
        if let Some(performance_mode) = patch.performance_mode {
            latest.performance_mode = performance_mode.as_str().to_string();
        }
        if let Some(guardian_mode) = patch.guardian_mode {
            latest.guardian_mode = guardian_mode.as_str().to_string();
        }
        if let Some(guardian_idle_integrity_enabled) = patch.guardian_idle_integrity_enabled {
            latest.guardian_idle_integrity_enabled = guardian_idle_integrity_enabled;
        }
        if let Some(theme) = patch.theme {
            latest.theme = theme.as_str().to_string();
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
            latest.music_track = music_track;
        }
        Ok(())
    })
    .await?;
    if sync_offline_username {
        application::sync_active_offline_account_from_username(state, &config.username)
            .await
            .map_err(config_account_sync_error_response)?;
    }
    Ok(current_config(state))
}

pub(super) async fn persist_config_mutation<Mutation>(
    state: &AppState,
    failure_telemetry: ConfigFailureTelemetry,
    mutation: Mutation,
) -> Result<AppConfig, ApiError>
where
    Mutation: FnOnce(&mut AppConfig) -> Result<(), ConfigStoreError> + Send + 'static,
{
    state.mutate_config(mutation).await.map_err(|error| {
        emit_config_save_failed(state, &error, failure_telemetry);
        config_update_error_response(error)
    })
}

fn emit_config_save_failed(
    state: &AppState,
    error: &ConfigStoreError,
    failure_telemetry: ConfigFailureTelemetry,
) {
    if failure_telemetry == ConfigFailureTelemetry::SuppressForConsentDisable
        || matches!(error, ConfigStoreError::Validation(_))
        || !state.config().current().telemetry_enabled
    {
        return;
    }
    state.telemetry().emit(TelemetryEvent::error_captured(
        TelemetryErrorKind::ConfigSaveFailed,
        TelemetryErrorArea::Config,
        TelemetryErrorLevel::Error,
        CONFIG_SAVE_ERROR_MESSAGE,
    ));
}

impl ConfigView {
    fn from_snapshot((revision, config): (u64, AppConfig)) -> Self {
        Self {
            revision,
            username: config.username,
            launch_auth_mode: ConfigLaunchAuthMode::parse(&config.launch_auth_mode)
                .expect("normalized config has a supported launch auth mode"),
            max_memory_mb: config.max_memory_mb,
            min_memory_mb: config.min_memory_mb,
            java_path_override: config.java_path_override,
            window_width: config.window_width,
            window_height: config.window_height,
            onboarding_done: config.onboarding_done,
            jvm_preset: ConfigJvmPreset::parse(&config.jvm_preset)
                .expect("normalized config has a supported JVM preset"),
            performance_mode: ConfigPerformanceMode::parse(&config.performance_mode)
                .expect("normalized config has a supported performance mode"),
            guardian_mode: ConfigGuardianMode::parse(&config.guardian_mode)
                .expect("normalized config has a supported Guardian mode"),
            guardian_idle_integrity_enabled: config.guardian_idle_integrity_enabled,
            theme: ConfigTheme::parse(&config.theme)
                .expect("normalized config has a supported theme"),
            custom_hue: config.custom_hue,
            custom_vibrancy: config.custom_vibrancy,
            lightness: config.lightness,
            telemetry_enabled: config.telemetry_enabled,
            discord_rpc_enabled: config.discord_rpc_enabled,
            discord_rpc_onboarding_seen: config.discord_rpc_onboarding_seen,
            music_enabled: config.music_enabled,
            music_volume: config.music_volume,
            music_track: config.music_track,
        }
    }
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
        state::{AppState, AppStateInit, IdleSweepTerminal, InstallStore, SessionStore},
    };
    use axial_config::{
        AppConfig, AppConfigValidationError, AppPaths, ConfigStore, ConfigStoreError, ConfigTheme,
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
    fn config_patch_rejects_unknown_semantic_values() {
        for (field, value) in [
            ("launch_auth_mode", "guest"),
            ("jvm_preset", "fastest"),
            ("performance_mode", "automatic"),
            ("guardian_mode", "legacy"),
            ("theme", "private-theme"),
        ] {
            assert!(
                serde_json::from_value::<ConfigPatch>(serde_json::json!({ (field): value }))
                    .is_err(),
                "{field} must use the closed public vocabulary"
            );
        }
    }

    #[test]
    fn config_view_omits_internal_storage_and_exposes_a_revision() {
        let fixture = TestFixture::new("public-view");
        fixture.seed_config(AppConfig {
            telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
            feature_overrides: [("developer.inspector".to_string(), true)].into(),
            library_dir: "/private/library".to_string(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        });

        let body = serde_json::to_value(super::current_config(&fixture.state))
            .expect("serialize public config view");

        assert_eq!(body["revision"], 1);
        assert_eq!(body["theme"], ConfigTheme::Default.as_str());
        for internal in [
            "telemetry_install_id",
            "feature_overrides",
            "library_dir",
            "library_mode",
        ] {
            assert!(
                body.get(internal).is_none(),
                "{internal} must remain internal"
            );
        }
    }

    #[tokio::test]
    async fn invalid_extreme_patch_rejects_before_persistence() {
        let fixture = TestFixture::new("invalid-extreme");
        let result = super::update_config(
            &fixture.state,
            ConfigPatch {
                max_memory_mb: Some(i32::MAX),
                ..ConfigPatch::default()
            },
        )
        .await;

        let (status, _) = result.expect_err("extreme memory must reject");
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(fixture.state.config().current(), AppConfig::default());
        assert!(!fixture.root.join("config.json").exists());
        assert_eq!(fixture.state.telemetry().queue_len_for_test(), 0);
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
        assert_eq!(config.revision, 1);
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
        let initial_idle = *fixture.state.subscribe_integrity_idle().borrow();
        let reservation = fixture
            .state
            .try_reserve_idle_sweep(
                initial_idle.epoch(),
                fixture
                    .state
                    .try_claim_producer()
                    .expect("claim config invalidation producer"),
            )
            .expect("reserve sweep before config commit");
        let cancellation = reservation.cancellation();

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
        assert!(cancellation.is_cancelled());
        assert!(!reservation.is_current());
        reservation.settle(IdleSweepTerminal::Cancelled);
        assert!(
            !fixture
                .state
                .config()
                .current()
                .guardian_idle_integrity_enabled
        );
        let disabled_epoch = fixture.state.subscribe_integrity_idle().borrow().epoch();
        assert_ne!(disabled_epoch, initial_idle.epoch());

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
        assert_ne!(
            fixture.state.subscribe_integrity_idle().borrow().epoch(),
            disabled_epoch
        );
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
        fixture.block_config_file();

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
        fixture.block_config_file();

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

        fixture.unblock_config_file();
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
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::from_config(
                    paths.clone(),
                    Arc::clone(&root_session),
                    AppConfig::default(),
                )
                .expect("set config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
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
                        PerformanceManager::load_for_startup(paths.performance_dir())
                            .expect("performance manager"),
                    ),
                    startup_warnings: Vec::new(),
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

        fn block_config_file(&self) {
            let path = self.root.join("config.json");
            let _ = fs::remove_file(&path);
            fs::create_dir_all(path).expect("block config file with directory");
        }

        fn unblock_config_file(&self) {
            fs::remove_dir_all(self.root.join("config.json")).expect("remove config file blocker");
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
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
