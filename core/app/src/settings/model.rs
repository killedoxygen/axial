use serde::{Deserialize, Deserializer, Serialize};
use ts_rs::TS;

use super::SettingsError;

macro_rules! setting_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
        pub enum $name { $(#[serde(rename = $value)] $variant),+ }
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($value => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

setting_enum!(ConfigLaunchAuthMode { Offline => "offline", Online => "online" });
setting_enum!(ConfigPerformanceMode { Managed => "managed", Vanilla => "vanilla", Custom => "custom" });
setting_enum!(ConfigTheme {
    Default => "", Obsidian => "obsidian", Deepslate => "deepslate", Nether => "nether",
    End => "end", Birch => "birch", Custom => "custom"
});
setting_enum!(ConfigJvmPreset {
    Automatic => "", Smooth => "smooth", Performance => "performance",
    UltraLowLatency => "ultra_low_latency", GraalVm => "graalvm", Legacy => "legacy",
    LegacyPvp => "legacy_pvp", LegacyHeavy => "legacy_heavy"
});

/// Public preferences. Library authority, telemetry identity and flag overrides
/// have separate owners/projections and never appear in this response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ConfigView {
    pub revision: u64,
    /// Projected by the account owner. Never used as persisted selection authority.
    #[serde(default)]
    pub account_selection_revision: u64,
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

impl Default for ConfigView {
    fn default() -> Self {
        Self {
            revision: 0,
            account_selection_revision: 0,
            username: "Player".into(),
            launch_auth_mode: ConfigLaunchAuthMode::Offline,
            max_memory_mb: 4096,
            min_memory_mb: 512,
            java_path_override: String::new(),
            window_width: 0,
            window_height: 0,
            onboarding_done: false,
            jvm_preset: ConfigJvmPreset::Automatic,
            performance_mode: ConfigPerformanceMode::Managed,
            theme: ConfigTheme::Default,
            custom_hue: None,
            custom_vibrancy: None,
            lightness: None,
            telemetry_enabled: false,
            discord_rpc_enabled: true,
            discord_rpc_onboarding_seen: false,
            music_enabled: None,
            music_volume: None,
            music_track: 0,
        }
    }
}

/// Missing means no edit; explicit null resets a nullable preference.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum NullablePatch<T> {
    #[default]
    Unchanged,
    Set(Option<T>),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NullablePatch<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(deserializer).map(Self::Set)
    }
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(de: D) -> Result<Option<T>, D::Error> {
    T::deserialize(de).map(Some)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    pub expected_revision: u64,
    #[serde(default, deserialize_with = "present")]
    pub expected_account_selection_revision: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    pub username: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub launch_auth_mode: Option<ConfigLaunchAuthMode>,
    #[serde(default, deserialize_with = "present")]
    pub max_memory_mb: Option<i32>,
    #[serde(default, deserialize_with = "present")]
    pub min_memory_mb: Option<i32>,
    #[serde(default, deserialize_with = "present")]
    pub java_path_override: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub window_width: Option<i32>,
    #[serde(default, deserialize_with = "present")]
    pub window_height: Option<i32>,
    #[serde(default, deserialize_with = "present")]
    pub onboarding_done: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub jvm_preset: Option<ConfigJvmPreset>,
    #[serde(default, deserialize_with = "present")]
    pub performance_mode: Option<ConfigPerformanceMode>,
    #[serde(default, deserialize_with = "present")]
    pub theme: Option<ConfigTheme>,
    #[serde(default)]
    pub custom_hue: NullablePatch<i32>,
    #[serde(default)]
    pub custom_vibrancy: NullablePatch<i32>,
    #[serde(default)]
    pub lightness: NullablePatch<i32>,
    #[serde(default, deserialize_with = "present")]
    pub telemetry_enabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub discord_rpc_enabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub discord_rpc_onboarding_seen: Option<bool>,
    #[serde(default)]
    pub music_enabled: NullablePatch<bool>,
    #[serde(default)]
    pub music_volume: NullablePatch<i32>,
    #[serde(default, deserialize_with = "present")]
    pub music_track: Option<i32>,
}

impl ConfigPatch {
    pub(super) fn apply(self, config: &mut ConfigView) -> Result<(), SettingsError> {
        // This command edits the offline name, never a provider-owned profile.
        if let Some(username) = self.username.as_deref() {
            validate_username(username.trim())?;
        }
        macro_rules! set {
            ($($field:ident),+ $(,)?) => { $(if let Some(value) = self.$field { config.$field = value; })+ };
        }
        set!(
            username,
            launch_auth_mode,
            max_memory_mb,
            min_memory_mb,
            java_path_override,
            window_width,
            window_height,
            onboarding_done,
            jvm_preset,
            performance_mode,
            theme,
            telemetry_enabled,
            discord_rpc_enabled,
            discord_rpc_onboarding_seen,
            music_track
        );
        macro_rules! nullable {
            ($($field:ident),+ $(,)?) => { $(if let NullablePatch::Set(value) = self.$field { config.$field = value; })+ };
        }
        nullable!(
            custom_hue,
            custom_vibrancy,
            lightness,
            music_enabled,
            music_volume
        );
        config.username = config.username.trim().to_owned();
        config.java_path_override = config.java_path_override.trim().to_owned();
        // A mode change may temporarily leave the previous account's name in
        // place. Validate that pair after the account owner projects selection.
        config.validate_preferences()
    }
}

impl ConfigView {
    pub fn validate(&self) -> Result<(), SettingsError> {
        match self.launch_auth_mode {
            ConfigLaunchAuthMode::Offline => validate_username(&self.username)?,
            ConfigLaunchAuthMode::Online => ensure(
                (1..=16).contains(&self.username.len())
                    && self
                        .username
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "Authenticated profile name must contain 1 to 16 letters, numbers, or underscores.",
            )?,
        }
        self.validate_preferences()
    }

    fn validate_preferences(&self) -> Result<(), SettingsError> {
        ensure(
            (512..=32768).contains(&self.max_memory_mb),
            "Maximum memory must be between 512 and 32768 MiB.",
        )?;
        ensure(
            (256..=self.max_memory_mb).contains(&self.min_memory_mb),
            "Minimum memory must be between 256 MiB and maximum memory.",
        )?;
        ensure(
            (self.window_width == 0 && self.window_height == 0)
                || ((320..=8192).contains(&self.window_width)
                    && (320..=8192).contains(&self.window_height)),
            "Window dimensions must both be zero or between 320 and 8192 pixels.",
        )?;
        validate_java_path(&self.java_path_override)?;
        optional_range(
            self.custom_hue,
            360,
            "Custom hue must be between 0 and 360.",
        )?;
        optional_range(
            self.custom_vibrancy,
            100,
            "Custom vibrancy must be between 0 and 100.",
        )?;
        optional_range(self.lightness, 100, "Lightness must be between 0 and 100.")?;
        optional_range(
            self.music_volume,
            100,
            "Music volume must be between 0 and 100.",
        )?;
        ensure(
            (0..=1023).contains(&self.music_track),
            "Music track must be between 0 and 1023.",
        )
    }
}

pub fn validate_username(value: &str) -> Result<(), SettingsError> {
    ensure(
        (3..=16).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "Username must contain 3 to 16 letters, numbers, or underscores.",
    )
}

fn validate_java_path(value: &str) -> Result<(), SettingsError> {
    ensure(
        value.len() <= 4096
            && value.encode_utf16().count() <= 4096
            && !value.chars().any(char::is_control),
        "Java path override is invalid or exceeds 4096 bytes.",
    )
}

fn optional_range(
    value: Option<i32>,
    max: i32,
    message: &'static str,
) -> Result<(), SettingsError> {
    ensure(value.is_none_or(|v| (0..=max).contains(&v)), message)
}

fn ensure(valid: bool, message: &'static str) -> Result<(), SettingsError> {
    if valid {
        Ok(())
    } else {
        Err(SettingsError::Validation(message))
    }
}

/// Instance-owned persisted overrides. Zero/empty keeps the inherited global
/// value, matching existing settings controls. No filesystem authority is held.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InstanceSettings {
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub java_path: String,
    pub window_width: i32,
    pub window_height: i32,
    pub jvm_preset: String,
    pub performance_mode: String,
    pub extra_jvm_args: String,
    pub auto_optimize: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveLaunchSettings {
    pub global_config_revision: u64,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub java_path: String,
    pub window_width: i32,
    pub window_height: i32,
    pub jvm_preset: ConfigJvmPreset,
    pub performance_mode: ConfigPerformanceMode,
    pub extra_jvm_args: String,
    pub auto_optimize: bool,
}

impl InstanceSettings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        ensure(
            (0..=1024 * 1024).contains(&self.max_memory_mb)
                && (0..=1024 * 1024).contains(&self.min_memory_mb),
            "Instance memory bounds are invalid.",
        )?;
        ensure(
            (0..=16384).contains(&self.window_width) && (0..=16384).contains(&self.window_height),
            "Instance window bounds are invalid.",
        )?;
        validate_java_path(&self.java_path)?;
        ensure(
            ConfigJvmPreset::parse(self.jvm_preset.trim()).is_some(),
            "Unknown JVM preset.",
        )?;
        ensure(
            self.performance_mode.trim().is_empty()
                || ConfigPerformanceMode::parse(self.performance_mode.trim()).is_some(),
            "Unknown performance mode.",
        )?;
        ensure(
            self.extra_jvm_args.chars().count() <= 8192
                && !self.extra_jvm_args.chars().any(char::is_control),
            "Instance JVM arguments are invalid or too long.",
        )
    }

    /// Base inheritance only. Launch owns host/version-derived defaults and
    /// admission/probing of the selected Java path and explicit JVM arguments.
    pub fn effective(&self, global: &ConfigView) -> Result<EffectiveLaunchSettings, SettingsError> {
        self.validate()?;
        global.validate()?;
        let inherit = |local, fallback| if local > 0 { local } else { fallback };
        let max_memory_mb = inherit(self.max_memory_mb, global.max_memory_mb);
        Ok(EffectiveLaunchSettings {
            global_config_revision: global.revision,
            max_memory_mb,
            min_memory_mb: inherit(self.min_memory_mb, global.min_memory_mb).min(max_memory_mb),
            java_path: if self.java_path.trim().is_empty() {
                global.java_path_override.trim()
            } else {
                self.java_path.trim()
            }
            .into(),
            window_width: inherit(self.window_width, global.window_width),
            window_height: inherit(self.window_height, global.window_height),
            jvm_preset: if self.jvm_preset.trim().is_empty() {
                global.jvm_preset
            } else {
                ConfigJvmPreset::parse(self.jvm_preset.trim())
                    .ok_or(SettingsError::Validation("Unknown JVM preset."))?
            },
            performance_mode: if self.performance_mode.trim().is_empty() {
                global.performance_mode
            } else {
                ConfigPerformanceMode::parse(self.performance_mode.trim())
                    .ok_or(SettingsError::Validation("Unknown performance mode."))?
            },
            extra_jvm_args: self.extra_jvm_args.clone(),
            auto_optimize: self.auto_optimize,
        })
    }
}
