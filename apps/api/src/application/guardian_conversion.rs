use crate::guardian::GuardianMode;
use axial_launcher::GuardianMode as LauncherGuardianMode;

pub(super) fn api_guardian_mode(mode: LauncherGuardianMode) -> GuardianMode {
    match mode {
        LauncherGuardianMode::Managed => GuardianMode::Managed,
        LauncherGuardianMode::Custom => GuardianMode::Custom,
    }
}

pub(super) fn api_guardian_mode_from_config(value: &str) -> GuardianMode {
    match value.trim() {
        "custom" => GuardianMode::Custom,
        "disabled" => GuardianMode::Disabled,
        _ => GuardianMode::Managed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_preserve_boundary_semantics() {
        assert_eq!(
            api_guardian_mode(LauncherGuardianMode::Managed),
            GuardianMode::Managed
        );
        assert_eq!(
            api_guardian_mode(LauncherGuardianMode::Custom),
            GuardianMode::Custom
        );
        assert_eq!(
            api_guardian_mode_from_config("custom"),
            GuardianMode::Custom
        );
        assert_eq!(
            api_guardian_mode_from_config(" disabled "),
            GuardianMode::Disabled
        );
        assert_eq!(
            api_guardian_mode_from_config("managed"),
            GuardianMode::Managed
        );
        assert_eq!(
            api_guardian_mode_from_config("unknown"),
            GuardianMode::Managed
        );
        assert_eq!(api_guardian_mode_from_config(""), GuardianMode::Managed);
    }
}
