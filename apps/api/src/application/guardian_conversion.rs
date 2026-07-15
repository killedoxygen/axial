use crate::guardian::GuardianMode;
use axial_launcher::GuardianMode as LauncherGuardianMode;

pub(super) fn api_guardian_mode(mode: LauncherGuardianMode) -> GuardianMode {
    match mode {
        LauncherGuardianMode::Managed => GuardianMode::Managed,
        LauncherGuardianMode::Custom => GuardianMode::Custom,
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
    }
}
