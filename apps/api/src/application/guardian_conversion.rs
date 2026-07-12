use crate::guardian::{GuardianActionKind, GuardianMode};
use axial_launcher::{
    GuardianDecision as LauncherGuardianDecision, GuardianMode as LauncherGuardianMode,
};

pub(super) fn api_guardian_mode(mode: LauncherGuardianMode) -> GuardianMode {
    match mode {
        LauncherGuardianMode::Managed => GuardianMode::Managed,
        LauncherGuardianMode::Custom => GuardianMode::Custom,
    }
}

pub(super) fn launcher_guardian_decision(decision: GuardianActionKind) -> LauncherGuardianDecision {
    match decision {
        GuardianActionKind::Allow | GuardianActionKind::RecordOnly => {
            LauncherGuardianDecision::Allowed
        }
        GuardianActionKind::Warn => LauncherGuardianDecision::Warned,
        GuardianActionKind::Block => LauncherGuardianDecision::Blocked,
        GuardianActionKind::AskUser => {
            unreachable!("preflight boundary must resolve confirmation before launch conversion")
        }
        GuardianActionKind::Repair
        | GuardianActionKind::Retry
        | GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Fallback
        | GuardianActionKind::Quarantine => LauncherGuardianDecision::Intervened,
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
            launcher_guardian_decision(GuardianActionKind::Block),
            LauncherGuardianDecision::Blocked
        );
        assert_eq!(
            launcher_guardian_decision(GuardianActionKind::Repair),
            LauncherGuardianDecision::Intervened
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
