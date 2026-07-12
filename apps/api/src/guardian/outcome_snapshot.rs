use super::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairStatus, GuardianCopyRequest,
    GuardianDirective, GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
    GuardianLaunchRecoveryPlanRequest, GuardianManagedJavaReason, GuardianMode,
    GuardianPerformanceSupervisionRejection, GuardianRepairStatus, GuardianStripJvmArgsReason,
    GuardianUserOutcome, author_guardian_copy, plan_launch_recovery_directive,
};
use crate::state::contracts::OperationPhase;
use axial_launcher::LaunchFailureClass;
use serde::{Deserialize, Serialize};

const SNAPSHOT_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-outcome-copy-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_OUTCOME_COPY_SNAPSHOT";
const EXPECTED_CASE_COUNT: usize = 29;
const EXPECTED_CASE_IDS: [&str; EXPECTED_CASE_COUNT] = [
    "runtime_repair.repaired",
    "runtime_repair.blocked",
    "runtime_repair.failed",
    "runtime_repair.suppressed",
    "install_artifact_repair.repaired",
    "install_artifact_repair.blocked",
    "install_artifact_repair.failed",
    "install_artifact_repair.suppressed",
    "install_failure.download_retry",
    "install_failure.download_block",
    "install_failure.metadata_invalid",
    "install_failure.dependency_failed",
    "install_failure.runtime_unavailable",
    "install_failure.rosetta_required",
    "install_failure.permission_denied",
    "install_failure.temp_file_leftover",
    "install_failure.atomic_promotion_failed",
    "install_failure.unsafe_ownership",
    "launch_recovery_suppressed.managed_runtime",
    "launch_recovery_suppressed.strip_jvm_args",
    "launch_recovery_suppressed.downgrade_preset",
    "launch_recovery_suppressed.disable_custom_gc",
    "performance_rejection.unsafe_ownership",
    "performance_rejection.missing_journal",
    "performance_rejection.unsafe_public_boundary",
    "performance_rejection.guardian_blocked",
    "performance_rejection.fallback_unavailable",
    "performance_rejection.rollback_unavailable",
    "persisted_state.schema_invalid",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.outcome_copy.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianOutcomeCopySnapshot {
    schema: SnapshotSchema,
    cases: Vec<GuardianOutcomeCopyCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianOutcomeCopyCase {
    id: String,
    input: GuardianOutcomeCopyInput,
    outcome: GuardianUserOutcomeProjection,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
enum GuardianOutcomeCopyInput {
    RuntimeRepair {
        status: GuardianRepairStatus,
    },
    InstallArtifactRepair {
        status: GuardianArtifactRepairStatus,
    },
    InstallFailure {
        diagnosis: DiagnosisId,
        decision: GuardianActionKind,
        component: Option<String>,
        platform: Option<String>,
    },
    LaunchRecoverySuppressed {
        kind: RecoveryKindInput,
    },
    PerformanceRejection {
        rejection: PerformanceRejectionInput,
        phase: OperationPhase,
    },
    PersistedStateLoad {
        decision: GuardianActionKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum RecoveryKindInput {
    SwitchManagedRuntime,
    StripRawJvmArgs,
    DowngradePreset,
    DisableCustomGc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PerformanceRejectionInput {
    UnsafeOwnership,
    MissingJournal,
    UnsafePublicBoundary,
    GuardianBlocked,
    FallbackUnavailable,
    RollbackUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianUserOutcomeProjection {
    decision: GuardianActionKind,
    phase: OperationPhase,
    summary: String,
    details: Vec<String>,
    guidance: Vec<String>,
}

impl From<GuardianUserOutcome> for GuardianUserOutcomeProjection {
    fn from(outcome: GuardianUserOutcome) -> Self {
        Self {
            decision: outcome.decision(),
            phase: outcome.phase(),
            summary: outcome.summary().to_string(),
            details: outcome.details().to_vec(),
            guidance: outcome.guidance().to_vec(),
        }
    }
}

#[test]
fn checked_in_guardian_outcome_copy_is_byte_stable_and_complete() {
    let fixture = serde_json::from_str::<GuardianOutcomeCopySnapshot>(SNAPSHOT_FIXTURE)
        .expect("strict Guardian outcome copy fixture");
    assert_snapshot_coverage(&fixture);
    let replayed = replay_snapshot(&fixture);

    assert_eq!(fixture, replayed);
    let pretty = serde_json::to_string_pretty(&replayed).expect("serialize outcome copy snapshot");
    assert_eq!(format!("{pretty}\n"), SNAPSHOT_FIXTURE);
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_outcome_copy_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian outcome copy snapshot"
    );
    let committed = serde_json::from_str::<GuardianOutcomeCopySnapshot>(SNAPSHOT_FIXTURE)
        .expect("strict committed Guardian outcome copy fixture");
    assert_snapshot_coverage(&committed);
    let replayed = replay_snapshot(&committed);
    assert_snapshot_coverage(&replayed);
    let fixture = serde_json::to_string_pretty(&replayed)
        .expect("serialize regenerated Guardian outcome copy snapshot");
    std::fs::write(snapshot_fixture_path(), format!("{fixture}\n"))
        .expect("write regenerated Guardian outcome copy snapshot");
}

fn replay_snapshot(snapshot: &GuardianOutcomeCopySnapshot) -> GuardianOutcomeCopySnapshot {
    GuardianOutcomeCopySnapshot {
        schema: snapshot.schema,
        cases: snapshot
            .cases
            .iter()
            .map(|case| GuardianOutcomeCopyCase {
                id: case.id.clone(),
                input: case.input.clone(),
                outcome: render_outcome(&case.input),
            })
            .collect(),
    }
}

fn render_outcome(input: &GuardianOutcomeCopyInput) -> GuardianUserOutcomeProjection {
    match input {
        GuardianOutcomeCopyInput::RuntimeRepair { status } => author_guardian_copy(
            GuardianCopyRequest::runtime_repair(Some(DiagnosisId::ManagedRuntimeCorrupt), *status),
        )
        .expect("runtime repair copy rule")
        .into(),
        GuardianOutcomeCopyInput::InstallArtifactRepair { status } => {
            author_guardian_copy(GuardianCopyRequest::artifact_repair(
                DiagnosisId::LauncherManagedArtifactCorrupt,
                *status,
            ))
            .expect("artifact repair copy rule")
            .into()
        }
        GuardianOutcomeCopyInput::InstallFailure {
            diagnosis,
            decision,
            component,
            platform,
        } => {
            let evidence =
                install_failure_evidence(*diagnosis, component.as_deref(), platform.as_deref());
            author_guardian_copy(GuardianCopyRequest::install_failure(
                *diagnosis, *decision, &evidence,
            ))
            .expect("install failure copy rule")
            .into()
        }
        GuardianOutcomeCopyInput::LaunchRecoverySuppressed { kind } => {
            let plan = launch_recovery_plan(*kind);
            author_guardian_copy(GuardianCopyRequest::launch_recovery_suppressed(
                &plan.directive,
            ))
            .expect("launch recovery suppression copy")
            .into()
        }
        GuardianOutcomeCopyInput::PerformanceRejection { rejection, phase } => {
            author_guardian_copy(GuardianCopyRequest::performance_rejection(
                (*rejection).into(),
                *phase,
            ))
            .expect("performance rejection copy rule")
            .into()
        }
        GuardianOutcomeCopyInput::PersistedStateLoad { decision } => {
            author_guardian_copy(GuardianCopyRequest::persisted_state_load(
                DiagnosisId::PersistedStateSchemaInvalid,
                *decision,
            ))
            .expect("persisted state copy rule")
            .into()
        }
    }
}

fn install_failure_evidence(
    diagnosis: DiagnosisId,
    component: Option<&str>,
    platform: Option<&str>,
) -> Vec<GuardianInstallArtifactFailureEvidence> {
    let kind = match diagnosis {
        DiagnosisId::ManagedRuntimeUnavailableForPlatform => {
            GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform
        }
        DiagnosisId::ManagedRuntimeRosettaRequired => {
            GuardianInstallArtifactFailureKind::RuntimeRosettaRequired
        }
        _ => return Vec::new(),
    };
    let mut evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        None,
        "copy-snapshot-artifact",
        kind,
    );
    if let Some(component) = component {
        evidence = evidence.with_field("component", component);
    }
    if let Some(platform) = platform {
        evidence = evidence.with_field("platform", platform);
    }
    vec![evidence]
}

fn launch_recovery_plan(kind: RecoveryKindInput) -> super::GuardianLaunchRecoveryPlan {
    let (directive, failure_class) = match kind {
        RecoveryKindInput::SwitchManagedRuntime => (
            GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::StartupRecovery,
            },
            LaunchFailureClass::JavaRuntimeMismatch,
        ),
        RecoveryKindInput::StripRawJvmArgs => (
            GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            },
            LaunchFailureClass::JvmUnsupportedOption,
        ),
        RecoveryKindInput::DowngradePreset => (
            GuardianDirective::startup_preset_downgrade("balanced"),
            LaunchFailureClass::JvmUnsupportedOption,
        ),
        RecoveryKindInput::DisableCustomGc => (
            GuardianDirective::DisableCustomGc,
            LaunchFailureClass::JvmUnsupportedOption,
        ),
    };
    plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
        instance_id: "copy-snapshot-instance",
        mode: GuardianMode::Managed,
        directive,
        failure_class,
        user_intent_hash:
            "sha256.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa",
    })
    .expect("valid launch recovery snapshot plan")
}

impl From<PerformanceRejectionInput> for GuardianPerformanceSupervisionRejection {
    fn from(value: PerformanceRejectionInput) -> Self {
        match value {
            PerformanceRejectionInput::UnsafeOwnership => Self::UnsafeOwnership,
            PerformanceRejectionInput::MissingJournal => Self::MissingJournal,
            PerformanceRejectionInput::UnsafePublicBoundary => Self::UnsafePublicBoundary,
            PerformanceRejectionInput::GuardianBlocked => Self::GuardianBlocked,
            PerformanceRejectionInput::FallbackUnavailable => Self::FallbackUnavailable,
            PerformanceRejectionInput::RollbackUnavailable => Self::RollbackUnavailable,
        }
    }
}

impl PerformanceRejectionInput {
    fn id(self) -> &'static str {
        match self {
            Self::UnsafeOwnership => "unsafe_ownership",
            Self::MissingJournal => "missing_journal",
            Self::UnsafePublicBoundary => "unsafe_public_boundary",
            Self::GuardianBlocked => "guardian_blocked",
            Self::FallbackUnavailable => "fallback_unavailable",
            Self::RollbackUnavailable => "rollback_unavailable",
        }
    }
}

fn repair_status_id(status: GuardianRepairStatus) -> &'static str {
    match status {
        GuardianRepairStatus::Repaired => "repaired",
        GuardianRepairStatus::Blocked => "blocked",
        GuardianRepairStatus::Failed => "failed",
        GuardianRepairStatus::Suppressed => "suppressed",
    }
}

fn launch_recovery_kind_id(kind: RecoveryKindInput) -> &'static str {
    match kind {
        RecoveryKindInput::SwitchManagedRuntime => "managed_runtime",
        RecoveryKindInput::StripRawJvmArgs => "strip_jvm_args",
        RecoveryKindInput::DowngradePreset => "downgrade_preset",
        RecoveryKindInput::DisableCustomGc => "disable_custom_gc",
    }
}

fn assert_snapshot_coverage(snapshot: &GuardianOutcomeCopySnapshot) {
    assert_eq!(snapshot.schema, SnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), EXPECTED_CASE_COUNT);
    let ids = snapshot
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids.as_slice(), EXPECTED_CASE_IDS);

    let mut axis_counts = [0_usize; 6];
    for case in &snapshot.cases {
        assert_eq!(case.id, canonical_case_id(&case.input));
        let axis = match &case.input {
            GuardianOutcomeCopyInput::RuntimeRepair { .. } => 0,
            GuardianOutcomeCopyInput::InstallArtifactRepair { .. } => 1,
            GuardianOutcomeCopyInput::InstallFailure { .. } => 2,
            GuardianOutcomeCopyInput::LaunchRecoverySuppressed { .. } => 3,
            GuardianOutcomeCopyInput::PerformanceRejection { .. } => 4,
            GuardianOutcomeCopyInput::PersistedStateLoad { .. } => 5,
        };
        axis_counts[axis] += 1;
    }
    assert_eq!(axis_counts, [4, 4, 10, 4, 6, 1]);
}

fn canonical_case_id(input: &GuardianOutcomeCopyInput) -> String {
    match input {
        GuardianOutcomeCopyInput::RuntimeRepair { status } => {
            format!("runtime_repair.{}", repair_status_id(*status))
        }
        GuardianOutcomeCopyInput::InstallArtifactRepair { status } => {
            format!("install_artifact_repair.{}", status.as_persisted_id())
        }
        GuardianOutcomeCopyInput::InstallFailure {
            diagnosis,
            decision,
            component,
            platform,
        } => install_failure_case_id(
            *diagnosis,
            *decision,
            component.as_deref(),
            platform.as_deref(),
        )
        .to_string(),
        GuardianOutcomeCopyInput::LaunchRecoverySuppressed { kind } => format!(
            "launch_recovery_suppressed.{}",
            launch_recovery_kind_id(*kind)
        ),
        GuardianOutcomeCopyInput::PerformanceRejection { rejection, phase } => {
            assert_eq!(*phase, OperationPhase::Installing);
            format!("performance_rejection.{}", rejection.id())
        }
        GuardianOutcomeCopyInput::PersistedStateLoad { decision } => {
            assert_eq!(*decision, GuardianActionKind::Warn);
            "persisted_state.schema_invalid".to_string()
        }
    }
}

fn install_failure_case_id(
    diagnosis: DiagnosisId,
    decision: GuardianActionKind,
    component: Option<&str>,
    platform: Option<&str>,
) -> &'static str {
    match (diagnosis, decision, component, platform) {
        (DiagnosisId::DownloadUnavailable, GuardianActionKind::Retry, None, None) => {
            "install_failure.download_retry"
        }
        (DiagnosisId::DownloadUnavailable, GuardianActionKind::Block, None, None) => {
            "install_failure.download_block"
        }
        (DiagnosisId::InstallArtifactMetadataInvalid, GuardianActionKind::Block, None, None) => {
            "install_failure.metadata_invalid"
        }
        (DiagnosisId::InstallDependencyFailed, GuardianActionKind::Block, None, None) => {
            "install_failure.dependency_failed"
        }
        (
            DiagnosisId::ManagedRuntimeUnavailableForPlatform,
            GuardianActionKind::Block,
            Some("java-runtime-gamma"),
            Some("linux-riscv64"),
        ) => "install_failure.runtime_unavailable",
        (
            DiagnosisId::ManagedRuntimeRosettaRequired,
            GuardianActionKind::Block,
            Some("java-runtime-legacy"),
            None,
        ) => "install_failure.rosetta_required",
        (DiagnosisId::FilesystemPermissionDenied, GuardianActionKind::Block, None, None) => {
            "install_failure.permission_denied"
        }
        (DiagnosisId::TempFileLeftover, GuardianActionKind::Block, None, None) => {
            "install_failure.temp_file_leftover"
        }
        (DiagnosisId::AtomicPromotionFailed, GuardianActionKind::Block, None, None) => {
            "install_failure.atomic_promotion_failed"
        }
        (DiagnosisId::ArtifactOwnershipUnsafe, GuardianActionKind::Block, None, None) => {
            "install_failure.unsafe_ownership"
        }
        _ => panic!("outcome copy fixture contains an unsupported install failure coordinate"),
    }
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-outcome-copy-v1.json")
}
