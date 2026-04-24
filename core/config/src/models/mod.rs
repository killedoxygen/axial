use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const USERNAME_MIN_LEN: usize = 3;
pub const USERNAME_MAX_LEN: usize = 16;

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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AppConfigValidationError {
    #[error("invalid username: {0}")]
    InvalidUsername(&'static str),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub username: String,
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
    pub library_dir: String,
    #[serde(default)]
    pub library_mode: String,
    #[serde(default)]
    pub music_enabled: Option<bool>,
    #[serde(default)]
    pub music_volume: Option<i32>,
    #[serde(default)]
    pub music_track: i32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            username: "Player".to_string(),
            max_memory_mb: 4096,
            min_memory_mb: 512,
            java_path_override: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: "managed".to_string(),
            guardian_mode: "managed".to_string(),
            theme: String::new(),
            custom_hue: None,
            custom_vibrancy: None,
            lightness: None,
            onboarding_done: false,
            library_dir: String::new(),
            library_mode: "managed".to_string(),
            music_enabled: None,
            music_volume: None,
            music_track: 0,
        }
    }
}

impl AppConfig {
    pub fn normalized(mut self) -> Result<Self, AppConfigValidationError> {
        self.username =
            validate_username(&self.username).map_err(AppConfigValidationError::InvalidUsername)?;
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
            _ => "managed".to_string(),
        };
        if self.library_mode.is_empty() {
            self.library_mode = "managed".to_string();
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, AppConfigValidationError, validate_username};

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
    fn validate_username_trims_valid_names() {
        assert_eq!(
            validate_username("  Player_1  "),
            Ok("Player_1".to_string())
        );
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
