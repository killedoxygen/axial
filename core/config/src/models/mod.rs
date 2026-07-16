use crate::flags::find_flag;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const USERNAME_MIN_LEN: usize = 3;
pub const USERNAME_MAX_LEN: usize = 16;
pub const LAUNCH_AUTH_MODE_OFFLINE: &str = "offline";
pub const LAUNCH_AUTH_MODE_ONLINE: &str = "online";

pub fn validate_username(raw: &str) -> Result<String, &'static str> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("Enter a name.");
    }
    if value.len() < USERNAME_MIN_LEN {
        return Err("At least 3 characters.");
    }
    if value.len() > USERNAME_MAX_LEN {
        return Err("At most 16 characters.");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("Letters, numbers, and underscores only.");
    }
    Ok(value.to_string())
}

pub fn validate_launch_auth_mode(raw: &str) -> Result<String, &'static str> {
    match raw.trim() {
        LAUNCH_AUTH_MODE_OFFLINE => Ok(LAUNCH_AUTH_MODE_OFFLINE.to_string()),
        LAUNCH_AUTH_MODE_ONLINE => Ok(LAUNCH_AUTH_MODE_ONLINE.to_string()),
        _ => Err("Use offline or online."),
    }
}

fn default_launch_auth_mode() -> String {
    LAUNCH_AUTH_MODE_OFFLINE.to_string()
}

fn default_discord_rpc_enabled() -> bool {
    true
}

fn default_guardian_idle_integrity_enabled() -> bool {
    true
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AppConfigValidationError {
    #[error("invalid username: {0}")]
    InvalidUsername(&'static str),
    #[error("invalid launch auth mode: {0}")]
    InvalidLaunchAuthMode(&'static str),
    #[error("invalid telemetry install id")]
    InvalidTelemetryInstallId,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub username: String,
    #[serde(default = "default_launch_auth_mode")]
    pub launch_auth_mode: String,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    #[serde(default)]
    pub java_path_override: String,
    #[serde(default)]
    pub window_width: i32,
    #[serde(default)]
    pub window_height: i32,
    #[serde(default)]
    pub jvm_preset: String,
    #[serde(default)]
    pub performance_mode: String,
    #[serde(default)]
    pub guardian_mode: String,
    #[serde(default = "default_guardian_idle_integrity_enabled")]
    pub guardian_idle_integrity_enabled: bool,
    #[serde(default)]
    pub theme: String,
    #[serde(default)]
    pub custom_hue: Option<i32>,
    #[serde(default)]
    pub custom_vibrancy: Option<i32>,
    #[serde(default)]
    pub lightness: Option<i32>,
    #[serde(default)]
    pub onboarding_done: bool,
    #[serde(default)]
    pub telemetry_enabled: bool,
    #[serde(default)]
    pub telemetry_install_id: String,
    #[serde(default = "default_discord_rpc_enabled")]
    pub discord_rpc_enabled: bool,
    #[serde(default)]
    pub discord_rpc_onboarding_seen: bool,
    #[serde(default)]
    pub library_dir: String,
    #[serde(default)]
    pub library_mode: String,
    #[serde(default)]
    pub music_enabled: Option<bool>,
    #[serde(default)]
    pub music_volume: Option<i32>,
    #[serde(default)]
    pub music_track: i32,
    #[serde(default)]
    pub feature_overrides: BTreeMap<String, bool>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            username: "Player".to_string(),
            launch_auth_mode: LAUNCH_AUTH_MODE_OFFLINE.to_string(),
            max_memory_mb: 4096,
            min_memory_mb: 512,
            java_path_override: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: "managed".to_string(),
            guardian_mode: "managed".to_string(),
            guardian_idle_integrity_enabled: true,
            theme: String::new(),
            custom_hue: None,
            custom_vibrancy: None,
            lightness: None,
            onboarding_done: false,
            telemetry_enabled: false,
            telemetry_install_id: String::new(),
            discord_rpc_enabled: true,
            discord_rpc_onboarding_seen: false,
            library_dir: String::new(),
            library_mode: "managed".to_string(),
            music_enabled: None,
            music_volume: None,
            music_track: 0,
            feature_overrides: BTreeMap::new(),
        }
    }
}

impl AppConfig {
    pub fn normalized(mut self) -> Result<Self, AppConfigValidationError> {
        self.username =
            validate_username(&self.username).map_err(AppConfigValidationError::InvalidUsername)?;
        self.launch_auth_mode = validate_launch_auth_mode(&self.launch_auth_mode)
            .map_err(AppConfigValidationError::InvalidLaunchAuthMode)?;
        if self.max_memory_mb < 512 {
            self.max_memory_mb = 4096;
        }
        if self.min_memory_mb < 256 {
            self.min_memory_mb = 512;
        }
        if self.min_memory_mb > self.max_memory_mb {
            self.min_memory_mb = self.max_memory_mb;
        }
        if self.performance_mode.is_empty() {
            self.performance_mode = "managed".to_string();
        }
        self.guardian_mode = match self.guardian_mode.trim() {
            "custom" => "custom".to_string(),
            "disabled" => "disabled".to_string(),
            _ => "managed".to_string(),
        };
        if self.library_mode.is_empty() {
            self.library_mode = "managed".to_string();
        }
        self.telemetry_install_id = if self.telemetry_enabled {
            let install_id = self.telemetry_install_id.trim();
            if !install_id.is_empty() && !telemetry_install_id_has_uuid_shape(install_id) {
                return Err(AppConfigValidationError::InvalidTelemetryInstallId);
            }
            install_id.to_string()
        } else {
            String::new()
        };
        self.feature_overrides
            .retain(|key, _| find_flag(key).is_some());
        Ok(self)
    }
}

fn telemetry_install_id_has_uuid_shape(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }

    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, AppConfigValidationError, LAUNCH_AUTH_MODE_OFFLINE, LAUNCH_AUTH_MODE_ONLINE,
        validate_launch_auth_mode, validate_username,
    };
    use crate::FEATURE_FLAGS;

    #[test]
    fn normalized_clamps_min_memory_to_max_memory() {
        let config = AppConfig {
            min_memory_mb: 800,
            max_memory_mb: 600,
            ..AppConfig::default()
        }
        .normalized()
        .expect("valid config should normalize");

        assert_eq!(config.max_memory_mb, 600);
        assert_eq!(config.min_memory_mb, 600);
    }

    #[test]
    fn normalized_preserves_disabled_guardian_mode() {
        let config = AppConfig {
            guardian_mode: "  disabled  ".to_string(),
            ..AppConfig::default()
        }
        .normalized()
        .expect("disabled Guardian mode should normalize");

        assert_eq!(config.guardian_mode, "disabled");
    }

    #[test]
    fn validate_username_trims_valid_names() {
        assert_eq!(
            validate_username("  Player_1  "),
            Ok("Player_1".to_string())
        );
    }

    #[test]
    fn default_launch_auth_mode_is_offline() {
        assert_eq!(
            AppConfig::default().launch_auth_mode,
            LAUNCH_AUTH_MODE_OFFLINE
        );
    }

    #[test]
    fn guardian_idle_integrity_defaults_to_enabled() {
        assert!(AppConfig::default().guardian_idle_integrity_enabled);
    }

    #[test]
    fn missing_launch_auth_mode_deserializes_to_offline() {
        let config = serde_json::from_value::<AppConfig>(serde_json::json!({
            "username": "Player",
            "max_memory_mb": 4096,
            "min_memory_mb": 512
        }))
        .expect("missing auth mode should deserialize");

        assert_eq!(config.launch_auth_mode, LAUNCH_AUTH_MODE_OFFLINE);
        assert!(!config.telemetry_enabled);
        assert!(config.telemetry_install_id.is_empty());
        assert!(config.discord_rpc_enabled);
        assert!(!config.discord_rpc_onboarding_seen);
        assert!(config.guardian_idle_integrity_enabled);
        assert_eq!(
            config
                .normalized()
                .expect("config should normalize")
                .launch_auth_mode,
            LAUNCH_AUTH_MODE_OFFLINE
        );
    }

    #[test]
    fn guardian_idle_integrity_can_be_explicitly_disabled() {
        let config = serde_json::from_value::<AppConfig>(serde_json::json!({
            "username": "Player",
            "max_memory_mb": 4096,
            "min_memory_mb": 512,
            "guardian_idle_integrity_enabled": false
        }))
        .expect("explicit idle integrity setting should deserialize");

        assert!(!config.guardian_idle_integrity_enabled);
        assert_eq!(
            serde_json::to_value(config)
                .expect("idle integrity setting should serialize")
                .get("guardian_idle_integrity_enabled"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn missing_feature_overrides_deserializes_to_empty_map() {
        let config = serde_json::from_value::<AppConfig>(serde_json::json!({
            "username": "Player",
            "max_memory_mb": 4096,
            "min_memory_mb": 512
        }))
        .expect("missing feature overrides should deserialize");

        assert!(config.feature_overrides.is_empty());
    }

    #[test]
    fn normalized_prunes_unknown_feature_overrides() {
        let known_key = FEATURE_FLAGS[0].key;
        let config = AppConfig {
            feature_overrides: [
                (known_key.to_string(), true),
                ("retired.flag".to_string(), true),
            ]
            .into(),
            ..AppConfig::default()
        }
        .normalized()
        .expect("config should normalize");

        assert_eq!(config.feature_overrides.len(), 1);
        assert_eq!(config.feature_overrides.get(known_key), Some(&true));
        assert!(!config.feature_overrides.contains_key("retired.flag"));
    }

    #[test]
    fn normalized_trims_valid_and_rejects_invalid_telemetry_install_id() {
        let config = AppConfig {
            telemetry_enabled: true,
            telemetry_install_id: "  123e4567-e89b-12d3-a456-426614174000  ".to_string(),
            ..AppConfig::default()
        }
        .normalized()
        .expect("config should normalize");

        assert_eq!(
            config.telemetry_install_id,
            "123e4567-e89b-12d3-a456-426614174000"
        );

        for invalid in [
            "123e4567e89b12d3a456426614174000",
            "123e4567-e89b-12d3-a456-42661417400z",
            "not-a-uuid",
        ] {
            let error = AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: invalid.to_string(),
                ..AppConfig::default()
            }
            .normalized()
            .expect_err("invalid install id should be rejected");

            assert_eq!(error, AppConfigValidationError::InvalidTelemetryInstallId);
        }
    }

    #[test]
    fn normalized_accepts_supported_launch_auth_modes_only() {
        assert_eq!(
            validate_launch_auth_mode(" online "),
            Ok(LAUNCH_AUTH_MODE_ONLINE.to_string())
        );
        assert_eq!(
            AppConfig {
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            }
            .normalized()
            .expect("online mode should normalize")
            .launch_auth_mode,
            LAUNCH_AUTH_MODE_ONLINE
        );

        for value in ["", "ONLINE", "microsoft", "legacy"] {
            let err = AppConfig {
                launch_auth_mode: value.to_string(),
                ..AppConfig::default()
            }
            .normalized()
            .expect_err("unsupported auth mode should fail");

            assert_eq!(
                err,
                AppConfigValidationError::InvalidLaunchAuthMode("Use offline or online.")
            );
        }
    }

    #[test]
    fn normalized_rejects_invalid_username() {
        let err = AppConfig {
            username: "bad name".to_string(),
            ..AppConfig::default()
        }
        .normalized()
        .expect_err("invalid username should be rejected");

        assert_eq!(
            err,
            AppConfigValidationError::InvalidUsername("Letters, numbers, and underscores only.")
        );
    }
}
