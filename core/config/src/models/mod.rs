use serde::{Deserialize, Serialize};

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
    pub fn normalized(mut self) -> Self {
        if self.username.is_empty() {
            self.username = "Player".to_string();
        }
        if self.max_memory_mb < 512 {
            self.max_memory_mb = 4096;
        }
        if self.min_memory_mb < 256 {
            self.min_memory_mb = 512;
        }
        if self.performance_mode.is_empty() {
            self.performance_mode = "managed".to_string();
        }
        if self.library_mode.is_empty() {
            self.library_mode = "managed".to_string();
        }
        self
    }
}
