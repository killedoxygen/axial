use croopor_launcher::{
    PRESET_GRAALVM, PRESET_LEGACY, PRESET_LEGACY_HEAVY, PRESET_LEGACY_PVP, PRESET_PERFORMANCE,
    PRESET_SMOOTH, PRESET_ULTRA_LOW_LATENCY,
};
use serde::{Deserialize, Serialize};

const AUTO_PRESET_ID: &str = "";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianJvmPresetOption {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianJvmPresetResolution {
    pub stored_preset: String,
    pub state_id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub explicit: bool,
    pub warning: bool,
}

pub fn guardian_jvm_preset_options() -> Vec<GuardianJvmPresetOption> {
    preset_catalog()
        .iter()
        .map(|preset| GuardianJvmPresetOption {
            id: preset.id.to_string(),
            label: preset.label.to_string(),
            detail: preset.detail.to_string(),
            default: preset.id == AUTO_PRESET_ID,
            disabled_reason: None,
        })
        .collect()
}

pub fn normalize_create_jvm_preset(value: Option<&str>) -> GuardianJvmPresetResolution {
    let requested = value.unwrap_or_default().trim();
    if requested.is_empty() || requested.eq_ignore_ascii_case("auto") {
        return GuardianJvmPresetResolution {
            stored_preset: String::new(),
            state_id: "automatic".to_string(),
            label: "Automatic JVM preset".to_string(),
            detail: None,
            explicit: false,
            warning: false,
        };
    }

    if let Some(preset) = preset_catalog()
        .iter()
        .find(|preset| preset.id == requested)
    {
        return GuardianJvmPresetResolution {
            stored_preset: preset.id.to_string(),
            state_id: "explicit_supported".to_string(),
            label: preset.label.to_string(),
            detail: Some("Guardian will re-check this preset before launch.".to_string()),
            explicit: true,
            warning: false,
        };
    }

    GuardianJvmPresetResolution {
        stored_preset: String::new(),
        state_id: "unknown_reset_to_auto".to_string(),
        label: "Automatic JVM preset".to_string(),
        detail: Some(
            "Guardian reset an unknown JVM preset to Automatic so launch safety stays backend-owned."
                .to_string(),
        ),
        explicit: false,
        warning: true,
    }
}

#[derive(Clone, Copy)]
struct PresetCatalogEntry {
    id: &'static str,
    label: &'static str,
    detail: &'static str,
}

fn preset_catalog() -> &'static [PresetCatalogEntry] {
    &[
        PresetCatalogEntry {
            id: AUTO_PRESET_ID,
            label: "Auto",
            detail: "Croopor picks safe JVM flags automatically.",
        },
        PresetCatalogEntry {
            id: PRESET_SMOOTH,
            label: "Smooth",
            detail: "Balances throughput and steady frame times.",
        },
        PresetCatalogEntry {
            id: PRESET_PERFORMANCE,
            label: "Performance",
            detail: "Pushes higher throughput on modern hardware.",
        },
        PresetCatalogEntry {
            id: PRESET_ULTRA_LOW_LATENCY,
            label: "Low latency",
            detail: "Shortens JVM pauses, sometimes trading peak FPS.",
        },
        PresetCatalogEntry {
            id: PRESET_GRAALVM,
            label: "GraalVM",
            detail: "Uses flags intended for GraalVM-based Java runtimes.",
        },
        PresetCatalogEntry {
            id: PRESET_LEGACY,
            label: "Legacy",
            detail: "Keeps conservative flags for older Minecraft and Java stacks.",
        },
        PresetCatalogEntry {
            id: PRESET_LEGACY_PVP,
            label: "Legacy PvP",
            detail: "Legacy tuning biased toward fast input response.",
        },
        PresetCatalogEntry {
            id: PRESET_LEGACY_HEAVY,
            label: "Legacy heavy",
            detail: "Legacy tuning for larger heaps and heavier old modpacks.",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_preset_normalization_accepts_auto_forms() {
        for value in [None, Some(""), Some("   "), Some("auto")] {
            let resolution = normalize_create_jvm_preset(value);
            assert_eq!(resolution.stored_preset, "");
            assert!(!resolution.explicit);
            assert!(!resolution.warning);
        }
    }

    #[test]
    fn create_preset_normalization_accepts_known_supported_id() {
        let resolution = normalize_create_jvm_preset(Some(PRESET_PERFORMANCE));

        assert_eq!(resolution.stored_preset, PRESET_PERFORMANCE);
        assert_eq!(resolution.state_id, "explicit_supported");
        assert!(resolution.explicit);
        assert!(!resolution.warning);
    }

    #[test]
    fn create_preset_normalization_resets_unknown_without_echoing_value() {
        let resolution =
            normalize_create_jvm_preset(Some(r"C:\Users\Alice\java.exe --accessToken secret"));

        assert_eq!(resolution.stored_preset, "");
        assert_eq!(resolution.state_id, "unknown_reset_to_auto");
        assert!(resolution.warning);
        let public = serde_json::to_string(&resolution).expect("serialize resolution");
        assert!(!public.contains("Alice"));
        assert!(!public.contains("accessToken"));
        assert!(!public.contains("secret"));
        assert!(!public.contains("java.exe"));
    }
}
