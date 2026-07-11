use crate::guardian::{GuardianDecisionKind, GuardianMode};
use axial_launcher::{
    GuardianDecision as LauncherGuardianDecision, GuardianMode as LauncherGuardianMode,
};

pub(super) fn api_guardian_mode(mode: LauncherGuardianMode) -> GuardianMode {
    match mode {
        LauncherGuardianMode::Managed => GuardianMode::Managed,
        LauncherGuardianMode::Custom => GuardianMode::Custom,
    }
}

pub(super) fn launcher_guardian_decision(
    decision: GuardianDecisionKind,
) -> LauncherGuardianDecision {
    match decision {
        GuardianDecisionKind::Allow | GuardianDecisionKind::RecordOnly => {
            LauncherGuardianDecision::Allowed
        }
        GuardianDecisionKind::Warn => LauncherGuardianDecision::Warned,
        GuardianDecisionKind::Block | GuardianDecisionKind::AskUser => {
            LauncherGuardianDecision::Blocked
        }
        GuardianDecisionKind::Repair
        | GuardianDecisionKind::Retry
        | GuardianDecisionKind::Replace
        | GuardianDecisionKind::Strip
        | GuardianDecisionKind::Downgrade
        | GuardianDecisionKind::Degrade
        | GuardianDecisionKind::Fallback
        | GuardianDecisionKind::Quarantine
        | GuardianDecisionKind::Rollback => LauncherGuardianDecision::Intervened,
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
            launcher_guardian_decision(GuardianDecisionKind::Block),
            LauncherGuardianDecision::Blocked
        );
        assert_eq!(
            launcher_guardian_decision(GuardianDecisionKind::AskUser),
            LauncherGuardianDecision::Blocked
        );
        assert_eq!(
            launcher_guardian_decision(GuardianDecisionKind::Repair),
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
