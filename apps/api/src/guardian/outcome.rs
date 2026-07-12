use super::{GuardianActionKind, GuardianLaunchRecoveryKind, GuardianLaunchRecoveryPlan};
use crate::state::contracts::OperationPhase;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianUserOutcome {
    pub decision: GuardianActionKind,
    pub phase: OperationPhase,
    pub summary: String,
    pub details: Vec<String>,
    pub guidance: Vec<String>,
}

pub fn launch_recovery_suppressed_user_outcome(
    plan: &GuardianLaunchRecoveryPlan,
) -> GuardianUserOutcome {
    let detail = format!(
        "Guardian suppressed a repeated launch self-healing retry for {} because the same recovery failed recently.",
        launch_recovery_public_action_label(plan.directive.kind)
    );
    GuardianUserOutcome {
        decision: GuardianActionKind::Block,
        phase: OperationPhase::Repairing,
        summary: detail.clone(),
        details: vec![detail],
        guidance: vec![
            "Review the latest game log or change the affected launch setting before retrying."
                .to_string(),
        ],
    }
}

pub fn launch_recovery_public_action_label(kind: GuardianLaunchRecoveryKind) -> &'static str {
    match kind {
        GuardianLaunchRecoveryKind::SwitchManagedRuntime => "managed Java recovery",
        GuardianLaunchRecoveryKind::StripRawJvmArgs => "explicit JVM argument recovery",
        GuardianLaunchRecoveryKind::DowngradePreset => "JVM preset recovery",
        GuardianLaunchRecoveryKind::DisableCustomGc => "custom GC flag recovery",
    }
}

#[cfg(test)]
mod tests {
    use super::launch_recovery_suppressed_user_outcome;
    use crate::guardian::{
        GuardianActionKind, GuardianLaunchRecoveryDirective, GuardianLaunchRecoveryEffect,
        GuardianLaunchRecoveryKind, GuardianLaunchRecoveryPlanRequest,
        plan_launch_recovery_directive,
    };
    use crate::state::contracts::OperationPhase;

    #[test]
    fn launch_recovery_suppression_outcome_authors_public_copy() {
        let plan = plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
            instance_id: "instance-1",
            mode: crate::guardian::GuardianMode::Managed,
            directive: GuardianLaunchRecoveryDirective {
                kind: GuardianLaunchRecoveryKind::StripRawJvmArgs,
                effect: GuardianLaunchRecoveryEffect::StripRawJvmArgs,
                description: "Guardian removed incompatible explicit JVM args before launch"
                    .to_string(),
            },
            failure_class: axial_launcher::LaunchFailureClass::JvmUnsupportedOption,
            user_intent_hash:
                "sha256.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa",
        })
        .expect("recovery plan");

        let outcome = launch_recovery_suppressed_user_outcome(&plan);

        assert_eq!(outcome.decision, GuardianActionKind::Block);
        assert_eq!(outcome.phase, OperationPhase::Repairing);
        assert_eq!(
            outcome.summary,
            "Guardian suppressed a repeated launch self-healing retry for explicit JVM argument recovery because the same recovery failed recently."
        );
        assert_eq!(outcome.details, vec![outcome.summary.clone()]);
        assert_eq!(
            outcome.guidance,
            vec![
                "Review the latest game log or change the affected launch setting before retrying."
                    .to_string()
            ]
        );
    }
}
