use serde::{Deserialize, Serialize};

pub const LAUNCH_MEMORY_HEADROOM_MB: u64 = 2048;
pub const LAUNCH_DISK_HEADROOM_MB: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardianMode {
    #[default]
    Managed,
    Custom,
}

impl GuardianMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Custom => "custom",
        }
    }

    pub fn from_config(value: &str) -> Self {
        match value.trim() {
            "custom" => Self::Custom,
            _ => Self::Managed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideOrigin {
    Global,
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LaunchGuardianContext {
    pub mode: GuardianMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_override_origin: Option<OverrideOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_override_origin: Option<OverrideOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_jvm_args_origin: Option<OverrideOrigin>,
}

impl LaunchGuardianContext {
    pub fn has_java_override(&self) -> bool {
        self.java_override_origin.is_some()
    }

    pub fn has_named_preset(&self) -> bool {
        self.preset_override_origin.is_some()
    }

    pub fn has_raw_jvm_args(&self) -> bool {
        self.raw_jvm_args_origin.is_some()
    }

    pub fn has_risky_overrides(&self) -> bool {
        self.has_java_override() || self.has_named_preset() || self.has_raw_jvm_args()
    }
}

#[cfg(test)]
mod tests {
    use super::{GuardianMode, LaunchGuardianContext, OverrideOrigin};

    #[test]
    fn named_preset_counts_as_risky_override_for_warning_policy() {
        let context = LaunchGuardianContext {
            mode: GuardianMode::Custom,
            java_override_origin: None,
            preset_override_origin: Some(OverrideOrigin::Instance),
            raw_jvm_args_origin: None,
        };

        assert!(context.has_risky_overrides());
    }
}
