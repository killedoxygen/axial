use super::decision_snapshot::{PolicyDecisionCell, decision_projection};
use super::launch_decision::preset_adjustment_snapshot;
use super::{
    Diagnosis, GuardianActionKind, GuardianConfidence, GuardianDecision, GuardianDomain,
    GuardianFactId, GuardianLaunchFailureOutcome, GuardianLaunchRecoveryDirective,
    GuardianLaunchRecoveryEffect, GuardianLaunchRecoveryKind, GuardianMode,
    GuardianPrepareFailureRequest, GuardianPresetAdjustmentRequest, GuardianSeverity,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, SafetyCase,
    guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
    guardian_startup_failure_outcome,
};
use crate::state::contracts::OwnershipClass;
use axial_launcher::LaunchFailureClass;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const SNAPSHOT_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-launch-boundary-snapshot-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_LAUNCH_BOUNDARY_SNAPSHOT";
const PREPARE_CASE_COUNT: usize = 24;
const STARTUP_CASE_COUNT: usize = 25;
const PRESET_CASE_COUNT: usize = 6;
const CASE_COUNT: usize = PREPARE_CASE_COUNT + STARTUP_CASE_COUNT + PRESET_CASE_COUNT;
const EXPECTED_CASE_IDS: [&str; CASE_COUNT] = [
    "prepare--unknown--managed-default",
    "prepare--jvm_unsupported_option--managed-default",
    "prepare--jvm_experimental_unlock--managed-default",
    "prepare--jvm_option_ordering--managed-default",
    "prepare--java_runtime_mismatch--managed-default",
    "prepare--out_of_memory--managed-default",
    "prepare--graphics_driver_crash--managed-default",
    "prepare--missing_dependency--managed-default",
    "prepare--mod_transformation_failure--managed-default",
    "prepare--mod_attributed_crash--managed-default",
    "prepare--classpath_module_conflict--managed-default",
    "prepare--launcher_managed_artifact_signature--managed-default",
    "prepare--auth_mode_incompatible--managed-default",
    "prepare--loader_bootstrap_failure--managed-default",
    "prepare--startup_stalled--managed-default",
    "prepare--java-runtime-mismatch--managed-recover",
    "prepare--java-runtime-mismatch--custom-explicit",
    "prepare--java-runtime-mismatch--managed-already-applied",
    "prepare--jvm-unsupported-option--managed-recover",
    "prepare--jvm-unsupported-option--custom-explicit",
    "prepare--jvm-unsupported-option--managed-already-applied",
    "prepare--jvm-unsupported-option--disabled",
    "prepare--jvm-experimental-unlock--managed-recover",
    "prepare--jvm-option-ordering--managed-recover",
    "startup--exited-unknown--managed-default",
    "startup--exited-jvm_unsupported_option--managed-default",
    "startup--exited-jvm_experimental_unlock--managed-default",
    "startup--exited-jvm_option_ordering--managed-default",
    "startup--exited-java_runtime_mismatch--managed-default",
    "startup--exited-out_of_memory--managed-default",
    "startup--exited-graphics_driver_crash--managed-default",
    "startup--exited-missing_dependency--managed-default",
    "startup--exited-mod_transformation_failure--managed-default",
    "startup--exited-mod_attributed_crash--managed-default",
    "startup--exited-classpath_module_conflict--managed-default",
    "startup--exited-launcher_managed_artifact_signature--managed-default",
    "startup--exited-auth_mode_incompatible--managed-default",
    "startup--exited-loader_bootstrap_failure--managed-default",
    "startup--exited-startup_stalled--managed-default",
    "startup--stalled-observation--managed",
    "startup--jvm-unsupported-option--managed-downgrade",
    "startup--jvm-unsupported-option--managed-disable-gc",
    "startup--jvm-unsupported-option--managed-gc-already-disabled",
    "startup--jvm-unsupported-option--managed-recovery-already-applied",
    "startup--jvm-unsupported-option--custom-explicit",
    "startup--jvm-unsupported-option--disabled",
    "startup--java-runtime-mismatch--managed-recover",
    "startup--java-runtime-mismatch--custom-explicit",
    "startup--java-runtime-mismatch--managed-already-applied",
    "preset--managed-explicit-changed",
    "preset--managed-owned-changed",
    "preset--custom-explicit-changed",
    "preset--disabled-explicit-changed",
    "preset--managed-unchanged",
    "preset--managed-empty-requested",
];

const FAILURE_CLASSES: [LaunchFailureClass; 15] = [
    LaunchFailureClass::Unknown,
    LaunchFailureClass::JvmUnsupportedOption,
    LaunchFailureClass::JvmExperimentalUnlock,
    LaunchFailureClass::JvmOptionOrdering,
    LaunchFailureClass::JavaRuntimeMismatch,
    LaunchFailureClass::OutOfMemory,
    LaunchFailureClass::GraphicsDriverCrash,
    LaunchFailureClass::MissingDependency,
    LaunchFailureClass::ModTransformationFailure,
    LaunchFailureClass::ModAttributedCrash,
    LaunchFailureClass::ClasspathModuleConflict,
    LaunchFailureClass::LauncherManagedArtifactSignature,
    LaunchFailureClass::AuthModeIncompatible,
    LaunchFailureClass::LoaderBootstrapFailure,
    LaunchFailureClass::StartupStalled,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.launch_boundary_snapshot.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianLaunchBoundarySnapshot {
    schema: SnapshotSchema,
    cases: Vec<BoundaryCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryCase {
    id: String,
    surface: BoundarySurface,
    mode: GuardianMode,
    applicable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure_class: Option<LaunchFailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnosis: Option<DiagnosisProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<PolicyDecisionCell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directive: Option<DirectiveProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BoundarySurface {
    Prepare,
    Startup,
    Preset,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosisProjection {
    id: super::DiagnosisId,
    domain: GuardianDomain,
    severity: GuardianSeverity,
    confidence: GuardianConfidence,
    ownership: OwnershipClass,
    fact_ids: Vec<GuardianFactId>,
    candidate_actions: Vec<GuardianActionKind>,
    public_reason_template: String,
}

impl From<&Diagnosis> for DiagnosisProjection {
    fn from(diagnosis: &Diagnosis) -> Self {
        Self {
            id: diagnosis.id,
            domain: diagnosis.domain,
            severity: diagnosis.severity,
            confidence: diagnosis.confidence,
            ownership: diagnosis.ownership,
            fact_ids: diagnosis.fact_ids.clone(),
            candidate_actions: diagnosis.candidate_actions.clone(),
            public_reason_template: diagnosis.public_reason_template.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectiveProjection {
    kind: GuardianLaunchRecoveryKind,
    effect: DirectiveEffectProjection,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum DirectiveEffectProjection {
    ForceManagedRuntime,
    StripRawJvmArgs,
    DowngradePreset { preset: String },
    DisableCustomGc,
}

#[test]
fn checked_in_guardian_launch_boundary_snapshot_is_byte_stable_and_complete() {
    let fixture = serde_json::from_str::<GuardianLaunchBoundarySnapshot>(SNAPSHOT_FIXTURE)
        .expect("strict Guardian launch boundary snapshot fixture");
    let generated = build_snapshot();
    let first = snapshot_bytes(&generated);
    let second = snapshot_bytes(&build_snapshot());

    assert_snapshot_coverage(&fixture);
    assert_eq!(fixture, generated);
    assert_eq!(first, second);
    assert_eq!(first.as_slice(), SNAPSHOT_FIXTURE.as_bytes());
}

#[test]
fn guardian_launch_boundary_snapshot_rejects_nested_directive_fields() {
    let mut value = serde_json::from_str::<serde_json::Value>(SNAPSHOT_FIXTURE)
        .expect("launch boundary snapshot JSON");
    let downgrade = value["cases"]
        .as_array_mut()
        .expect("snapshot cases")
        .iter_mut()
        .find_map(|case| {
            case.get_mut("directive")?
                .get_mut("effect")?
                .get_mut("DowngradePreset")?
                .as_object_mut()
        })
        .expect("downgrade directive effect");
    downgrade.insert("unexpected".to_string(), serde_json::Value::Bool(true));

    let error = serde_json::from_value::<GuardianLaunchBoundarySnapshot>(value)
        .expect_err("nested directive fields must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_launch_boundary_snapshot_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian launch boundary snapshot"
    );
    std::fs::write(snapshot_fixture_path(), snapshot_bytes(&build_snapshot()))
        .expect("write regenerated Guardian launch boundary snapshot");
}

fn build_snapshot() -> GuardianLaunchBoundarySnapshot {
    let mut cases = prepare_cases();
    cases.extend(startup_cases());
    cases.extend(preset_cases());
    cases.sort_by(|left, right| left.id.cmp(&right.id));
    GuardianLaunchBoundarySnapshot {
        schema: SnapshotSchema::V1,
        cases,
    }
}

fn prepare_cases() -> Vec<BoundaryCase> {
    let mut cases = FAILURE_CLASSES
        .into_iter()
        .map(|failure_class| {
            prepare_case(
                format!("prepare--{}--managed-default", failure_class.as_str()),
                GuardianPrepareFailureRequest {
                    mode: GuardianMode::Managed,
                    failure_class,
                    public_error: "Launch preparation failed.",
                    requested_java_present: false,
                    explicit_java_override_present: false,
                    explicit_jvm_args_present: false,
                    runtime_intervention_applied: false,
                    raw_jvm_args_intervention_applied: false,
                },
            )
        })
        .collect::<Vec<_>>();

    cases.extend([
        prepare_case(
            "prepare--java-runtime-mismatch--managed-recover",
            prepare_request(
                GuardianMode::Managed,
                LaunchFailureClass::JavaRuntimeMismatch,
            )
            .with_java_recovery(false),
        ),
        prepare_case(
            "prepare--java-runtime-mismatch--custom-explicit",
            prepare_request(
                GuardianMode::Custom,
                LaunchFailureClass::JavaRuntimeMismatch,
            )
            .with_java_recovery(false),
        ),
        prepare_case(
            "prepare--java-runtime-mismatch--managed-already-applied",
            prepare_request(
                GuardianMode::Managed,
                LaunchFailureClass::JavaRuntimeMismatch,
            )
            .with_java_recovery(true),
        ),
        prepare_case(
            "prepare--jvm-unsupported-option--managed-recover",
            prepare_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_jvm_recovery(false),
        ),
        prepare_case(
            "prepare--jvm-unsupported-option--custom-explicit",
            prepare_request(
                GuardianMode::Custom,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_jvm_recovery(false),
        ),
        prepare_case(
            "prepare--jvm-unsupported-option--managed-already-applied",
            prepare_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_jvm_recovery(true),
        ),
        prepare_case(
            "prepare--jvm-unsupported-option--disabled",
            prepare_request(
                GuardianMode::Disabled,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_jvm_recovery(false),
        ),
        prepare_case(
            "prepare--jvm-experimental-unlock--managed-recover",
            prepare_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmExperimentalUnlock,
            )
            .with_jvm_recovery(false),
        ),
        prepare_case(
            "prepare--jvm-option-ordering--managed-recover",
            prepare_request(GuardianMode::Managed, LaunchFailureClass::JvmOptionOrdering)
                .with_jvm_recovery(false),
        ),
    ]);
    assert_eq!(cases.len(), PREPARE_CASE_COUNT);
    cases
}

fn startup_cases() -> Vec<BoundaryCase> {
    let mut cases = FAILURE_CLASSES
        .into_iter()
        .map(|failure_class| {
            startup_case(
                format!(
                    "startup--exited-{}--managed-default",
                    failure_class.as_str()
                ),
                GuardianStartupFailureRequest {
                    mode: GuardianMode::Managed,
                    observation: GuardianStartupFailureObservation::Exited { failure_class },
                    crash_evidence: None,
                    target_version_id: "1.21.1",
                    runtime_major: 21,
                    requested_java_present: false,
                    explicit_java_override_present: false,
                    explicit_jvm_args_present: false,
                    explicit_jvm_preset_present: false,
                    startup_recovery_applied: false,
                    disable_custom_gc: true,
                    effective_preset: "",
                },
            )
        })
        .collect::<Vec<_>>();

    cases.extend([
        startup_case(
            "startup--stalled-observation--managed",
            startup_request(
                GuardianMode::Managed,
                GuardianStartupFailureObservation::Stalled,
            ),
        ),
        startup_case(
            "startup--jvm-unsupported-option--managed-downgrade",
            startup_exited_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_preset("ultra_low_latency", false, false),
        ),
        startup_case(
            "startup--jvm-unsupported-option--managed-disable-gc",
            startup_exited_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_preset("performance", false, false),
        ),
        startup_case(
            "startup--jvm-unsupported-option--managed-gc-already-disabled",
            startup_exited_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_preset("performance", true, false),
        ),
        startup_case(
            "startup--jvm-unsupported-option--managed-recovery-already-applied",
            startup_exited_request(
                GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_preset("ultra_low_latency", false, true),
        ),
        startup_case(
            "startup--jvm-unsupported-option--custom-explicit",
            startup_exited_request(
                GuardianMode::Custom,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_explicit_preset("ultra_low_latency"),
        ),
        startup_case(
            "startup--jvm-unsupported-option--disabled",
            startup_exited_request(
                GuardianMode::Disabled,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .with_explicit_preset("ultra_low_latency"),
        ),
        startup_case(
            "startup--java-runtime-mismatch--managed-recover",
            startup_exited_request(
                GuardianMode::Managed,
                LaunchFailureClass::JavaRuntimeMismatch,
            )
            .with_java_recovery(false),
        ),
        startup_case(
            "startup--java-runtime-mismatch--custom-explicit",
            startup_exited_request(
                GuardianMode::Custom,
                LaunchFailureClass::JavaRuntimeMismatch,
            )
            .with_java_recovery(false),
        ),
        startup_case(
            "startup--java-runtime-mismatch--managed-already-applied",
            startup_exited_request(
                GuardianMode::Managed,
                LaunchFailureClass::JavaRuntimeMismatch,
            )
            .with_java_recovery(true),
        ),
    ]);
    assert_eq!(cases.len(), STARTUP_CASE_COUNT);
    cases
}

fn preset_cases() -> Vec<BoundaryCase> {
    let cases = vec![
        preset_case(
            "preset--managed-explicit-changed",
            GuardianMode::Managed,
            "ultra_low_latency",
            "performance",
            true,
        ),
        preset_case(
            "preset--managed-owned-changed",
            GuardianMode::Managed,
            "ultra_low_latency",
            "performance",
            false,
        ),
        preset_case(
            "preset--custom-explicit-changed",
            GuardianMode::Custom,
            "ultra_low_latency",
            "performance",
            true,
        ),
        preset_case(
            "preset--disabled-explicit-changed",
            GuardianMode::Disabled,
            "ultra_low_latency",
            "performance",
            true,
        ),
        preset_case(
            "preset--managed-unchanged",
            GuardianMode::Managed,
            "performance",
            "performance",
            false,
        ),
        preset_case(
            "preset--managed-empty-requested",
            GuardianMode::Managed,
            "",
            "performance",
            false,
        ),
    ];
    assert_eq!(cases.len(), PRESET_CASE_COUNT);
    cases
}

fn prepare_case(id: impl Into<String>, request: GuardianPrepareFailureRequest<'_>) -> BoundaryCase {
    let mode = request.mode;
    let outcome = guardian_prepare_failure_outcome(request);
    outcome_case(
        id,
        BoundarySurface::Prepare,
        mode,
        outcome.failure_class,
        outcome,
    )
}

fn startup_case(id: impl Into<String>, request: GuardianStartupFailureRequest<'_>) -> BoundaryCase {
    let mode = request.mode;
    let outcome = guardian_startup_failure_outcome(request);
    outcome_case(
        id,
        BoundarySurface::Startup,
        mode,
        outcome.failure_class,
        outcome,
    )
}

fn outcome_case(
    id: impl Into<String>,
    surface: BoundarySurface,
    mode: GuardianMode,
    failure_class: LaunchFailureClass,
    outcome: GuardianLaunchFailureOutcome,
) -> BoundaryCase {
    let [diagnosis] = outcome.safety_case.diagnoses.as_slice() else {
        panic!("launch boundary must produce one diagnosis")
    };
    BoundaryCase {
        id: id.into(),
        surface,
        mode,
        applicable: true,
        failure_class: Some(failure_class),
        diagnosis: Some(DiagnosisProjection::from(diagnosis)),
        decision: Some(boundary_decision_projection(
            &outcome.guardian_decision,
            &outcome.safety_case,
        )),
        directive: outcome.directive.as_ref().map(DirectiveProjection::from),
    }
}

fn preset_case(
    id: impl Into<String>,
    mode: GuardianMode,
    requested_preset: &str,
    effective_preset: &str,
    explicit_jvm_preset_present: bool,
) -> BoundaryCase {
    let request = GuardianPresetAdjustmentRequest {
        mode,
        requested_preset,
        effective_preset,
        explicit_jvm_preset_present,
    };
    let evaluation = preset_adjustment_snapshot(&request);
    let applicable = evaluation.is_some();
    let (diagnosis, decision, directive) = match evaluation {
        Some((safety_case, decision, directive)) => {
            let [diagnosis] = safety_case.diagnoses.as_slice() else {
                panic!("preset boundary must produce one diagnosis")
            };
            (
                Some(DiagnosisProjection::from(diagnosis)),
                Some(boundary_decision_projection(&decision, &safety_case)),
                directive.as_ref().map(DirectiveProjection::from),
            )
        }
        None => (None, None, None),
    };
    let public_directive = guardian_prelaunch_preset_adjustment_directive(request);
    assert_eq!(
        public_directive.as_ref().map(DirectiveProjection::from),
        directive
    );
    BoundaryCase {
        id: id.into(),
        surface: BoundarySurface::Preset,
        mode,
        applicable,
        failure_class: None,
        diagnosis,
        decision,
        directive,
    }
}

fn boundary_decision_projection(
    decision: &GuardianDecision,
    safety_case: &SafetyCase,
) -> PolicyDecisionCell {
    decision_projection(decision, safety_case)
}

impl From<&GuardianLaunchRecoveryDirective> for DirectiveProjection {
    fn from(directive: &GuardianLaunchRecoveryDirective) -> Self {
        Self {
            kind: directive.kind,
            effect: match &directive.effect {
                GuardianLaunchRecoveryEffect::ForceManagedRuntime => {
                    DirectiveEffectProjection::ForceManagedRuntime
                }
                GuardianLaunchRecoveryEffect::StripRawJvmArgs => {
                    DirectiveEffectProjection::StripRawJvmArgs
                }
                GuardianLaunchRecoveryEffect::DowngradePreset { preset } => {
                    DirectiveEffectProjection::DowngradePreset {
                        preset: preset.clone(),
                    }
                }
                GuardianLaunchRecoveryEffect::DisableCustomGc => {
                    DirectiveEffectProjection::DisableCustomGc
                }
            },
        }
    }
}

fn assert_snapshot_coverage(snapshot: &GuardianLaunchBoundarySnapshot) {
    assert_eq!(snapshot.schema, SnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), CASE_COUNT);
    assert_eq!(
        snapshot
            .cases
            .iter()
            .filter(|case| case.surface == BoundarySurface::Prepare)
            .count(),
        PREPARE_CASE_COUNT
    );
    assert_eq!(
        snapshot
            .cases
            .iter()
            .filter(|case| case.surface == BoundarySurface::Startup)
            .count(),
        STARTUP_CASE_COUNT
    );
    assert_eq!(
        snapshot
            .cases
            .iter()
            .filter(|case| case.surface == BoundarySurface::Preset)
            .count(),
        PRESET_CASE_COUNT
    );
    let preset_cases = snapshot
        .cases
        .iter()
        .filter(|case| case.surface == BoundarySurface::Preset)
        .collect::<Vec<_>>();
    assert_eq!(
        preset_cases.iter().filter(|case| case.applicable).count(),
        4
    );
    assert_eq!(
        preset_cases.iter().filter(|case| !case.applicable).count(),
        2
    );

    let ids = snapshot
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        ids.iter().copied().collect::<HashSet<_>>().len(),
        CASE_COUNT
    );
    let mut expected_ids = EXPECTED_CASE_IDS.to_vec();
    expected_ids.sort_unstable();
    assert_eq!(ids, expected_ids);
    assert!(snapshot.cases.iter().all(|case| {
        case.applicable
            == (case.diagnosis.is_some()
                && case.decision.is_some()
                && case
                    .decision
                    .is_some_and(|decision| decision.plan_integrity))
    }));

    let prepare_defaults =
        default_failure_class_inventory(snapshot, "prepare--", "--managed-default");
    let startup_defaults =
        default_failure_class_inventory(snapshot, "startup--exited-", "--managed-default");
    let mut expected = FAILURE_CLASSES
        .into_iter()
        .map(LaunchFailureClass::as_str)
        .collect::<Vec<_>>();
    expected.sort_unstable();
    assert_eq!(prepare_defaults, expected);
    assert_eq!(startup_defaults, expected);

    let mut effects = [false; 4];
    for case in &snapshot.cases {
        let Some(directive) = &case.directive else {
            continue;
        };
        let decision = case.decision.expect("directive decision");
        assert_eq!(directive.kind.action_kind(), decision.decision_kind);
        assert!(matches!(
            (&directive.kind, &directive.effect),
            (
                GuardianLaunchRecoveryKind::SwitchManagedRuntime,
                DirectiveEffectProjection::ForceManagedRuntime
            ) | (
                GuardianLaunchRecoveryKind::StripRawJvmArgs,
                DirectiveEffectProjection::StripRawJvmArgs
            ) | (
                GuardianLaunchRecoveryKind::DowngradePreset,
                DirectiveEffectProjection::DowngradePreset { .. }
            ) | (
                GuardianLaunchRecoveryKind::DisableCustomGc,
                DirectiveEffectProjection::DisableCustomGc
            )
        ));
        match &directive.effect {
            DirectiveEffectProjection::ForceManagedRuntime => effects[0] = true,
            DirectiveEffectProjection::StripRawJvmArgs => effects[1] = true,
            DirectiveEffectProjection::DowngradePreset { .. } => effects[2] = true,
            DirectiveEffectProjection::DisableCustomGc => effects[3] = true,
        }
    }
    assert!(effects.into_iter().all(|seen| seen));

    assert!(snapshot.cases.iter().all(|case| {
        case.diagnosis.as_ref().is_none_or(|diagnosis| {
            !diagnosis.public_reason_template.is_empty()
                && diagnosis
                    .public_reason_template
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        })
    }));
    let bytes = snapshot_bytes(snapshot);
    let text = std::str::from_utf8(&bytes).expect("snapshot is UTF-8");
    assert!(!text.contains("description"));
    assert!(!text.contains("user_outcome"));
    assert!(!text.contains("Launch preparation failed"));
}

fn default_failure_class_inventory(
    snapshot: &GuardianLaunchBoundarySnapshot,
    prefix: &str,
    suffix: &str,
) -> Vec<&'static str> {
    let mut values = snapshot
        .cases
        .iter()
        .filter(|case| case.id.starts_with(prefix) && case.id.ends_with(suffix))
        .map(|case| {
            case.failure_class
                .expect("default case failure class")
                .as_str()
        })
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    assert_eq!(values.len(), FAILURE_CLASSES.len());
    values
}

fn snapshot_bytes(snapshot: &GuardianLaunchBoundarySnapshot) -> Vec<u8> {
    let pretty =
        serde_json::to_string_pretty(snapshot).expect("serialize launch boundary snapshot");
    format!("{pretty}\n").into_bytes()
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-launch-boundary-snapshot-v1.json")
}

fn prepare_request(
    mode: GuardianMode,
    failure_class: LaunchFailureClass,
) -> GuardianPrepareFailureRequest<'static> {
    GuardianPrepareFailureRequest {
        mode,
        failure_class,
        public_error: "Launch preparation failed.",
        requested_java_present: false,
        explicit_java_override_present: false,
        explicit_jvm_args_present: false,
        runtime_intervention_applied: false,
        raw_jvm_args_intervention_applied: false,
    }
}

trait PrepareRequestCases {
    fn with_java_recovery(self, intervention_applied: bool) -> Self;
    fn with_jvm_recovery(self, intervention_applied: bool) -> Self;
}

impl PrepareRequestCases for GuardianPrepareFailureRequest<'static> {
    fn with_java_recovery(mut self, intervention_applied: bool) -> Self {
        self.requested_java_present = true;
        self.explicit_java_override_present = true;
        self.runtime_intervention_applied = intervention_applied;
        self
    }

    fn with_jvm_recovery(mut self, intervention_applied: bool) -> Self {
        self.explicit_jvm_args_present = true;
        self.raw_jvm_args_intervention_applied = intervention_applied;
        self
    }
}

fn startup_request(
    mode: GuardianMode,
    observation: GuardianStartupFailureObservation,
) -> GuardianStartupFailureRequest<'static> {
    GuardianStartupFailureRequest {
        mode,
        observation,
        crash_evidence: None,
        target_version_id: "1.21.1",
        runtime_major: 21,
        requested_java_present: false,
        explicit_java_override_present: false,
        explicit_jvm_args_present: false,
        explicit_jvm_preset_present: false,
        startup_recovery_applied: false,
        disable_custom_gc: true,
        effective_preset: "",
    }
}

fn startup_exited_request(
    mode: GuardianMode,
    failure_class: LaunchFailureClass,
) -> GuardianStartupFailureRequest<'static> {
    startup_request(
        mode,
        GuardianStartupFailureObservation::Exited { failure_class },
    )
}

trait StartupRequestCases {
    fn with_preset(
        self,
        effective_preset: &'static str,
        disable_custom_gc: bool,
        recovery_applied: bool,
    ) -> Self;
    fn with_explicit_preset(self, effective_preset: &'static str) -> Self;
    fn with_java_recovery(self, recovery_applied: bool) -> Self;
}

impl StartupRequestCases for GuardianStartupFailureRequest<'static> {
    fn with_preset(
        mut self,
        effective_preset: &'static str,
        disable_custom_gc: bool,
        recovery_applied: bool,
    ) -> Self {
        self.effective_preset = effective_preset;
        self.disable_custom_gc = disable_custom_gc;
        self.startup_recovery_applied = recovery_applied;
        self
    }

    fn with_explicit_preset(mut self, effective_preset: &'static str) -> Self {
        self.effective_preset = effective_preset;
        self.disable_custom_gc = false;
        self.explicit_jvm_preset_present = true;
        self
    }

    fn with_java_recovery(mut self, recovery_applied: bool) -> Self {
        self.requested_java_present = true;
        self.explicit_java_override_present = true;
        self.startup_recovery_applied = recovery_applied;
        self
    }
}
