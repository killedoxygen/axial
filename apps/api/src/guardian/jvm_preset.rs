use axial_launcher::{
    PRESET_GRAALVM, PRESET_LEGACY, PRESET_LEGACY_HEAVY, PRESET_LEGACY_PVP, PRESET_PERFORMANCE,
    PRESET_SMOOTH, PRESET_ULTRA_LOW_LATENCY,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianJvmPresetId {
    Auto,
    Smooth,
    Performance,
    UltraLowLatency,
    GraalVm,
    Legacy,
    LegacyPvp,
    LegacyHeavy,
}

impl GuardianJvmPresetId {
    pub const ALL: [Self; 8] = [
        Self::Auto,
        Self::Smooth,
        Self::Performance,
        Self::UltraLowLatency,
        Self::GraalVm,
        Self::Legacy,
        Self::LegacyPvp,
        Self::LegacyHeavy,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "",
            Self::Smooth => PRESET_SMOOTH,
            Self::Performance => PRESET_PERFORMANCE,
            Self::UltraLowLatency => PRESET_ULTRA_LOW_LATENCY,
            Self::GraalVm => PRESET_GRAALVM,
            Self::Legacy => PRESET_LEGACY,
            Self::LegacyPvp => PRESET_LEGACY_PVP,
            Self::LegacyHeavy => PRESET_LEGACY_HEAVY,
        }
    }

    fn parse_explicit(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| *preset != Self::Auto && preset.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianJvmPresetResolution {
    Automatic,
    ExplicitSupported(GuardianJvmPresetId),
    UnknownResetToAutomatic,
}

impl GuardianJvmPresetResolution {
    pub const fn stored_preset(self) -> &'static str {
        match self {
            Self::ExplicitSupported(preset) => preset.as_str(),
            Self::Automatic | Self::UnknownResetToAutomatic => "",
        }
    }
}

pub fn normalize_create_jvm_preset(value: Option<&str>) -> GuardianJvmPresetResolution {
    let requested = value.unwrap_or_default().trim();
    if requested.is_empty() || requested.eq_ignore_ascii_case("auto") {
        return GuardianJvmPresetResolution::Automatic;
    }

    GuardianJvmPresetId::parse_explicit(requested)
        .map(GuardianJvmPresetResolution::ExplicitSupported)
        .unwrap_or(GuardianJvmPresetResolution::UnknownResetToAutomatic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_preset_normalization_accepts_auto_forms() {
        for value in [None, Some(""), Some("   "), Some("auto"), Some("AUTO")] {
            assert_eq!(
                normalize_create_jvm_preset(value),
                GuardianJvmPresetResolution::Automatic
            );
        }
    }

    #[test]
    fn create_preset_normalization_accepts_every_supported_id() {
        for preset in GuardianJvmPresetId::ALL {
            if preset == GuardianJvmPresetId::Auto {
                continue;
            }
            assert_eq!(
                normalize_create_jvm_preset(Some(preset.as_str())),
                GuardianJvmPresetResolution::ExplicitSupported(preset)
            );
        }
    }

    #[test]
    fn create_preset_normalization_resets_unknown_without_retaining_value() {
        let resolution =
            normalize_create_jvm_preset(Some(r"C:\Users\Alice\java.exe --accessToken secret"));

        assert_eq!(
            resolution,
            GuardianJvmPresetResolution::UnknownResetToAutomatic
        );
        assert_eq!(resolution.stored_preset(), "");
    }
}
