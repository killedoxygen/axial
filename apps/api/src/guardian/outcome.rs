use super::{
    DiagnosisId, GuardianActionKind, GuardianLaunchRecoveryKind, GuardianLaunchRecoveryPlan,
    GuardianPerformanceSupervisionRejection, GuardianRepairOutcome, GuardianRepairStatus,
};
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

pub fn runtime_repair_user_outcome(outcome: &GuardianRepairOutcome) -> GuardianUserOutcome {
    let decision = match outcome.status {
        GuardianRepairStatus::Repaired => GuardianActionKind::Repair,
        GuardianRepairStatus::Blocked
        | GuardianRepairStatus::Failed
        | GuardianRepairStatus::Suppressed => GuardianActionKind::Block,
    };
    let (summary, details, guidance) = runtime_repair_outcome_copy(outcome.status);
    GuardianUserOutcome {
        decision,
        phase: OperationPhase::Repairing,
        summary: summary.to_string(),
        details: details.into_iter().map(str::to_string).collect(),
        guidance: guidance.into_iter().map(str::to_string).collect(),
    }
}

pub fn install_artifact_repair_user_outcome(status: &str) -> GuardianUserOutcome {
    let normalized = status.trim();
    let decision = match normalized {
        "repaired" => GuardianActionKind::Repair,
        "blocked" | "failed" | "suppressed" => GuardianActionKind::Block,
        _ => GuardianActionKind::RecordOnly,
    };
    let (summary, detail) = install_artifact_repair_outcome_copy(normalized);

    GuardianUserOutcome {
        decision,
        phase: OperationPhase::Repairing,
        summary: summary.to_string(),
        details: detail.into_iter().map(str::to_string).collect(),
        guidance: Vec::new(),
    }
}

pub fn install_failure_user_outcome(
    decision: GuardianActionKind,
    diagnosis_id: DiagnosisId,
) -> GuardianUserOutcome {
    let (summary, details, guidance) = install_failure_outcome_copy(diagnosis_id, decision);
    GuardianUserOutcome {
        decision,
        phase: OperationPhase::Downloading,
        summary: summary.to_string(),
        details: details.into_iter().map(str::to_string).collect(),
        guidance: guidance.into_iter().map(str::to_string).collect(),
    }
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

pub fn performance_supervision_rejection_user_outcome(
    _error: GuardianPerformanceSupervisionRejection,
    phase: OperationPhase,
) -> GuardianUserOutcome {
    GuardianUserOutcome {
        decision: GuardianActionKind::Block,
        phase,
        summary: "performance update was blocked by Guardian safety supervision".to_string(),
        details: Vec::new(),
        guidance: Vec::new(),
    }
}

pub fn persisted_state_load_user_outcome(
    decision: GuardianActionKind,
    diagnosis_id: DiagnosisId,
) -> GuardianUserOutcome {
    let (summary, details, guidance) = persisted_state_load_outcome_copy(diagnosis_id);
    GuardianUserOutcome {
        decision,
        phase: OperationPhase::Startup,
        summary: summary.to_string(),
        details: details.into_iter().map(str::to_string).collect(),
        guidance: guidance.into_iter().map(str::to_string).collect(),
    }
}

fn runtime_repair_outcome_copy(
    status: GuardianRepairStatus,
) -> (&'static str, Vec<&'static str>, Vec<&'static str>) {
    match status {
        GuardianRepairStatus::Repaired => (
            "Guardian repaired launch state before launch.",
            vec!["Guardian repaired the managed Java runtime before launch."],
            Vec::new(),
        ),
        GuardianRepairStatus::Suppressed => (
            "Guardian blocked launch preflight.",
            vec![
                "Guardian suppressed managed Java runtime repair because the same repair failed recently.",
            ],
            vec!["Reinstall or repair the affected version/runtime before launching again."],
        ),
        GuardianRepairStatus::Failed => (
            "Guardian blocked launch preflight.",
            vec!["Guardian could not repair the managed Java runtime automatically."],
            vec!["Reinstall or repair the affected version/runtime before launching again."],
        ),
        GuardianRepairStatus::Blocked => (
            "Guardian blocked launch preflight.",
            vec!["Guardian blocked managed Java runtime repair because it was not safe to apply."],
            vec!["Reinstall or repair the affected version/runtime before launching again."],
        ),
    }
}

fn install_artifact_repair_outcome_copy(status: &str) -> (&'static str, Option<&'static str>) {
    match status {
        "repaired" => (
            "Guardian repaired a launcher-managed install artifact.",
            Some("Retry the install to continue from the repaired state."),
        ),
        "suppressed" => (
            "Guardian paused automatic install repair after repeated failure.",
            Some("Check connection and storage permissions before trying again."),
        ),
        "blocked" => (
            "Guardian blocked automatic install repair because it was unsafe.",
            Some("The launcher did not mutate files that were not proven launcher-managed."),
        ),
        "failed" => (
            "Guardian could not repair the launcher-managed install artifact.",
            Some("Check connection and storage permissions before trying again."),
        ),
        _ => (
            "Guardian recorded an install repair outcome.",
            Some("Check the install operation status before retrying."),
        ),
    }
}

fn install_failure_outcome_copy(
    diagnosis_id: DiagnosisId,
    decision: GuardianActionKind,
) -> (&'static str, Vec<&'static str>, Vec<&'static str>) {
    match diagnosis_id {
        DiagnosisId::DownloadUnavailable if decision == GuardianActionKind::Block => (
            "Guardian paused install retry after repeated provider failure.",
            vec![
                "The install stopped because the same provider or network download failure repeated within the retry cooldown.",
            ],
            vec![
                "Wait a few minutes, then retry after checking connection and storage availability.",
            ],
        ),
        DiagnosisId::DownloadUnavailable => (
            "Guardian classified the install download failure as retryable.",
            vec![
                "The install stopped because a provider or network download was unavailable or interrupted.",
            ],
            vec!["Retry the install after checking connection and storage availability."],
        ),
        DiagnosisId::InstallArtifactMetadataInvalid => (
            "Guardian blocked install because provider metadata could not be trusted.",
            vec!["The install did not continue with invalid provider metadata."],
            vec!["Retry later or choose another version source."],
        ),
        DiagnosisId::InstallDependencyFailed => (
            "Guardian blocked loader install because the required base install failed.",
            vec!["The loader install did not continue after the base Minecraft install failed."],
            vec!["Retry the base version install, then retry the loader install."],
        ),
        DiagnosisId::ManagedRuntimeUnavailableForPlatform => (
            "This Minecraft version needs a Java runtime that is not available for this device.",
            vec!["The required managed Java runtime is not available for this device."],
            vec!["This version cannot be installed on this device."],
        ),
        DiagnosisId::ManagedRuntimeRosettaRequired => (
            "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.",
            vec!["The required managed Java runtime needs Rosetta 2 on this Mac."],
            vec![
                "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.",
            ],
        ),
        DiagnosisId::FilesystemPermissionDenied => (
            "Guardian blocked install because Axial could not write launcher-managed files safely.",
            vec!["The install did not mutate files after the filesystem refused the operation."],
            vec!["Check app data permissions and retry the install."],
        ),
        DiagnosisId::TempFileLeftover => (
            "Guardian blocked install because temporary download state could not be written safely.",
            vec![
                "The install did not continue after temporary download state could not be written or cleaned safely.",
            ],
            vec!["Check app data permissions and disk availability before retrying the install."],
        ),
        DiagnosisId::AtomicPromotionFailed => (
            "Guardian blocked install because verified download data could not be promoted safely.",
            vec![
                "The install did not replace launcher-managed files after atomic promotion failed.",
            ],
            vec!["Check app data permissions and retry the install."],
        ),
        DiagnosisId::ArtifactOwnershipUnsafe => (
            "Guardian blocked install to protect user-owned or unknown files.",
            vec!["The install did not automatically mutate a target whose ownership was unsafe."],
            vec![
                "Move the affected files or choose a launcher-managed library location before retrying.",
            ],
        ),
        _ => (
            "Guardian recorded an install safety outcome.",
            vec!["The install failure was captured as bounded Guardian evidence."],
            vec!["Retry the install or inspect the latest operation status."],
        ),
    }
}

fn persisted_state_load_outcome_copy(
    diagnosis_id: DiagnosisId,
) -> (&'static str, Vec<&'static str>, Vec<&'static str>) {
    match diagnosis_id {
        DiagnosisId::PersistedStateSchemaInvalid => (
            "Guardian kept Axial running after persisted operation state could not be trusted.",
            vec!["Some restart-resume records were ignored instead of resuming unsafe work."],
            vec!["Retry the affected performance or benchmark operation if it is still needed."],
        ),
        _ => (
            "Guardian recorded a persisted state safety issue.",
            vec!["Axial ignored untrusted local operation state instead of using it."],
            vec!["Retry the affected operation if it is still needed."],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        install_artifact_repair_user_outcome, install_failure_user_outcome,
        launch_recovery_suppressed_user_outcome, performance_supervision_rejection_user_outcome,
        persisted_state_load_user_outcome, runtime_repair_user_outcome,
    };
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianLaunchRecoveryDirective,
        GuardianLaunchRecoveryEffect, GuardianLaunchRecoveryKind,
        GuardianLaunchRecoveryPlanRequest, GuardianPerformanceSupervisionRejection,
        GuardianRepairOutcome, GuardianRepairStatus, plan_launch_recovery_directive,
    };
    use crate::state::contracts::{OperationId, OperationPhase};

    #[test]
    fn runtime_repair_outcomes_author_public_copy() {
        let cases = [
            (
                GuardianRepairStatus::Repaired,
                GuardianActionKind::Repair,
                "Guardian repaired launch state before launch.",
                "Guardian repaired the managed Java runtime before launch.",
            ),
            (
                GuardianRepairStatus::Suppressed,
                GuardianActionKind::Block,
                "Guardian blocked launch preflight.",
                "Guardian suppressed managed Java runtime repair because the same repair failed recently.",
            ),
            (
                GuardianRepairStatus::Failed,
                GuardianActionKind::Block,
                "Guardian blocked launch preflight.",
                "Guardian could not repair the managed Java runtime automatically.",
            ),
            (
                GuardianRepairStatus::Blocked,
                GuardianActionKind::Block,
                "Guardian blocked launch preflight.",
                "Guardian blocked managed Java runtime repair because it was not safe to apply.",
            ),
        ];

        for (status, decision, summary, detail) in cases {
            let outcome = runtime_repair_user_outcome(&repair_outcome(status));

            assert_eq!(outcome.decision, decision);
            assert_eq!(outcome.phase, OperationPhase::Repairing);
            assert_eq!(outcome.summary, summary);
            assert_eq!(outcome.details, vec![detail.to_string()]);
            if decision == GuardianActionKind::Block {
                assert_eq!(
                    outcome.guidance,
                    vec![
                        "Reinstall or repair the affected version/runtime before launching again."
                            .to_string()
                    ]
                );
            }
        }
    }

    #[test]
    fn install_artifact_repair_outcomes_author_public_copy() {
        let cases = [
            (
                "repaired",
                GuardianActionKind::Repair,
                "Guardian repaired a launcher-managed install artifact.",
                "Retry the install to continue from the repaired state.",
            ),
            (
                "suppressed",
                GuardianActionKind::Block,
                "Guardian paused automatic install repair after repeated failure.",
                "Check connection and storage permissions before trying again.",
            ),
            (
                "blocked",
                GuardianActionKind::Block,
                "Guardian blocked automatic install repair because it was unsafe.",
                "The launcher did not mutate files that were not proven launcher-managed.",
            ),
            (
                "failed",
                GuardianActionKind::Block,
                "Guardian could not repair the launcher-managed install artifact.",
                "Check connection and storage permissions before trying again.",
            ),
            (
                "unknown",
                GuardianActionKind::RecordOnly,
                "Guardian recorded an install repair outcome.",
                "Check the install operation status before retrying.",
            ),
        ];

        for (status, decision, summary, detail) in cases {
            let outcome = install_artifact_repair_user_outcome(status);

            assert_eq!(outcome.decision, decision);
            assert_eq!(outcome.phase, OperationPhase::Repairing);
            assert_eq!(outcome.summary, summary);
            assert_eq!(outcome.details, vec![detail.to_string()]);
        }
    }

    #[test]
    fn install_failure_outcomes_author_public_copy() {
        let cases = [
            (
                DiagnosisId::DownloadUnavailable,
                GuardianActionKind::Retry,
                "Guardian classified the install download failure as retryable.",
                "provider or network download",
            ),
            (
                DiagnosisId::InstallArtifactMetadataInvalid,
                GuardianActionKind::Block,
                "Guardian blocked install because provider metadata could not be trusted.",
                "invalid provider metadata",
            ),
            (
                DiagnosisId::InstallDependencyFailed,
                GuardianActionKind::Block,
                "Guardian blocked loader install because the required base install failed.",
                "base Minecraft install failed",
            ),
            (
                DiagnosisId::ManagedRuntimeRosettaRequired,
                GuardianActionKind::Block,
                "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.",
                "Rosetta 2",
            ),
            (
                DiagnosisId::ManagedRuntimeUnavailableForPlatform,
                GuardianActionKind::Block,
                "This Minecraft version needs a Java runtime that is not available for this device.",
                "required managed Java runtime",
            ),
            (
                DiagnosisId::FilesystemPermissionDenied,
                GuardianActionKind::Block,
                "Guardian blocked install because Axial could not write launcher-managed files safely.",
                "filesystem refused",
            ),
            (
                DiagnosisId::TempFileLeftover,
                GuardianActionKind::Block,
                "Guardian blocked install because temporary download state could not be written safely.",
                "temporary download state",
            ),
            (
                DiagnosisId::AtomicPromotionFailed,
                GuardianActionKind::Block,
                "Guardian blocked install because verified download data could not be promoted safely.",
                "atomic promotion failed",
            ),
            (
                DiagnosisId::ArtifactOwnershipUnsafe,
                GuardianActionKind::Block,
                "Guardian blocked install to protect user-owned or unknown files.",
                "ownership was unsafe",
            ),
        ];

        for (diagnosis_id, decision, summary, detail_fragment) in cases {
            let outcome = install_failure_user_outcome(decision, diagnosis_id);

            assert_eq!(outcome.decision, decision);
            assert_eq!(outcome.phase, OperationPhase::Downloading);
            assert_eq!(outcome.summary, summary);
            assert!(
                outcome
                    .details
                    .iter()
                    .any(|detail| detail.contains(detail_fragment)),
                "{diagnosis_id} did not include expected detail"
            );
            assert!(!outcome.guidance.is_empty());
        }
    }

    #[test]
    fn persisted_state_load_outcome_authors_bounded_public_copy() {
        let outcome = persisted_state_load_user_outcome(
            GuardianActionKind::Warn,
            DiagnosisId::PersistedStateSchemaInvalid,
        );

        assert_eq!(outcome.decision, GuardianActionKind::Warn);
        assert_eq!(outcome.phase, OperationPhase::Startup);
        assert_eq!(
            outcome.summary,
            "Guardian kept Axial running after persisted operation state could not be trusted."
        );
        assert!(
            outcome
                .details
                .iter()
                .any(|detail| detail.contains("restart-resume records"))
        );
        assert!(outcome.guidance.iter().any(|detail| {
            detail.contains("Retry the affected performance or benchmark operation")
        }));
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        assert!(!encoded.contains("/home/"));
        assert!(!encoded.contains("C:\\"));
        assert!(!encoded.contains("line"));
        assert!(!encoded.contains("unexpected"));
    }

    #[test]
    fn launch_recovery_suppression_outcome_authors_public_copy() {
        let intent_fingerprint =
            "sha256.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa";
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
            user_intent_hash: intent_fingerprint,
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

    #[test]
    fn performance_supervision_rejection_outcome_authors_public_copy() {
        let outcome = performance_supervision_rejection_user_outcome(
            GuardianPerformanceSupervisionRejection::UnsafeOwnership,
            OperationPhase::Installing,
        );

        assert_eq!(outcome.decision, GuardianActionKind::Block);
        assert_eq!(outcome.phase, OperationPhase::Installing);
        assert_eq!(
            outcome.summary,
            "performance update was blocked by Guardian safety supervision"
        );
        assert!(outcome.details.is_empty());
        assert!(outcome.guidance.is_empty());
    }

    fn repair_outcome(status: GuardianRepairStatus) -> GuardianRepairOutcome {
        GuardianRepairOutcome {
            operation_id: OperationId::new("repair-operation"),
            diagnosis_id: Some(DiagnosisId::ManagedRuntimeCorrupt),
            action: Some(GuardianActionKind::Repair),
            status,
            facts: Vec::new(),
            summary: "managed_runtime_ready_marker_repaired".to_string(),
        }
    }
}
