use super::launch_decision::preset_adjustment_snapshot;
use super::launch_decision_snapshot::{
    LaunchBoundaryCopySource, committed_launch_boundary_case_ids, launch_boundary_copy_sources,
};
use super::{
    GuardianActionKind, GuardianCopyRequest, GuardianDirective, GuardianLaunchRecoveryPlanRequest,
    GuardianManagedJavaReason, GuardianMode, GuardianObservedLaunchFailurePhase,
    GuardianPrepareFailureRequest, GuardianPresetAdjustmentRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, GuardianStripJvmArgsReason,
    GuardianUserOutcome, author_guardian_copy, guardian_directive_description,
    guardian_prepare_failure_outcome, guardian_startup_failure_outcome,
    plan_launch_recovery_directive,
};
use crate::state::contracts::OperationPhase;
use axial_launcher::{CrashEvidence, LaunchFailureClass};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const COPY_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-launch-copy-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_LAUNCH_COPY_SNAPSHOT";
const BOUNDARY_CASE_COUNT: usize = 55;
const OBSERVED_CASE_COUNT: usize = 13;
const EXPECTED_CASE_COUNT: usize = 80;
const ACCEPTED_CRASH_CLASSES: [LaunchFailureClass; 5] = [
    LaunchFailureClass::OutOfMemory,
    LaunchFailureClass::GraphicsDriverCrash,
    LaunchFailureClass::MissingDependency,
    LaunchFailureClass::ModTransformationFailure,
    LaunchFailureClass::ModAttributedCrash,
];
const EXPECTED_SUPPLEMENTAL_IDS: [&str; 25] = [
    "observed.graphics_driver_crash.after_boot.generic",
    "observed.graphics_driver_crash.before_boot.generic",
    "observed.missing_dependency.after_boot.generic",
    "observed.missing_dependency.before_boot.generic",
    "observed.mod_attributed_crash.after_boot.generic",
    "observed.mod_attributed_crash.after_boot.public_safe_first",
    "observed.mod_attributed_crash.before_boot.generic",
    "observed.mod_attributed_crash.before_boot.public_safe_first",
    "observed.mod_transformation_failure.after_boot.generic",
    "observed.mod_transformation_failure.before_boot.generic",
    "observed.out_of_memory.after_boot.generic",
    "observed.out_of_memory.before_boot.generic",
    "observed.unknown.after_boot.generic",
    "prepare.public_error.hostile_unknown",
    "prepare.public_error.rejected_java_runtime_mismatch",
    "prepare.public_error.rejected_jvm_unsupported_option",
    "preset.bounded_labels",
    "recovery_suppressed.disable_custom_gc",
    "recovery_suppressed.downgrade_preset",
    "recovery_suppressed.managed_runtime",
    "recovery_suppressed.strip_jvm_args",
    "startup.guidance.generic_explicit_override",
    "startup.guidance.stalled_explicit_override",
    "startup.jvm_unsupported_option.legacy_preset_downgrade",
    "startup.mod_attributed_crash.public_safe_first",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.launch_copy.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianLaunchCopySnapshot {
    schema: SnapshotSchema,
    cases: Vec<GuardianLaunchCopyCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianLaunchCopyCase {
    id: String,
    input: GuardianLaunchCopyInput,
    output: GuardianLaunchCopyOutput,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
enum GuardianLaunchCopyInput {
    BoundaryReference {
        boundary_id: String,
    },
    ObservedCrash {
        failure_class: LaunchFailureClass,
        phase: ObservedPhaseInput,
        suspected_mods: Vec<String>,
    },
    PrepareFailure {
        mode: GuardianMode,
        failure_class: LaunchFailureClass,
        public_error: String,
        requested_java_present: bool,
        explicit_java_override_present: bool,
        explicit_jvm_args_present: bool,
        runtime_intervention_applied: bool,
        raw_jvm_args_intervention_applied: bool,
    },
    StartupFailure {
        mode: GuardianMode,
        observation: StartupObservationInput,
        suspected_mods: Vec<String>,
        target_version_id: String,
        runtime_major: u32,
        requested_java_present: bool,
        explicit_java_override_present: bool,
        explicit_jvm_args_present: bool,
        explicit_jvm_preset_present: bool,
        startup_recovery_applied: bool,
        disable_custom_gc: bool,
        effective_preset: String,
    },
    PresetAdjustment {
        mode: GuardianMode,
        requested_preset: String,
        effective_preset: String,
        explicit_jvm_preset_present: bool,
    },
    RecoverySuppressed {
        kind: RecoveryKindProjection,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ObservedPhaseInput {
    BeforeBoot,
    AfterBoot,
}

impl From<ObservedPhaseInput> for GuardianObservedLaunchFailurePhase {
    fn from(value: ObservedPhaseInput) -> Self {
        match value {
            ObservedPhaseInput::BeforeBoot => Self::BeforeBoot,
            ObservedPhaseInput::AfterBoot => Self::AfterBoot,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StartupObservationInput {
    Stalled,
    Exited { failure_class: LaunchFailureClass },
}

impl From<StartupObservationInput> for GuardianStartupFailureObservation {
    fn from(value: StartupObservationInput) -> Self {
        match value {
            StartupObservationInput::Stalled => Self::Stalled,
            StartupObservationInput::Exited { failure_class } => Self::Exited { failure_class },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianLaunchCopyOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authored_decision: Option<GuardianActionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<GuardianUserOutcomeProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directive: Option<DirectiveCopyProjection>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectiveCopyProjection {
    kind: RecoveryKindProjection,
    description: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum RecoveryKindProjection {
    SwitchManagedRuntime,
    StripRawJvmArgs,
    DowngradePreset,
    DisableCustomGc,
}

impl RecoveryKindProjection {
    fn action_kind(self) -> GuardianActionKind {
        match self {
            Self::SwitchManagedRuntime => GuardianActionKind::Fallback,
            Self::StripRawJvmArgs | Self::DisableCustomGc => GuardianActionKind::Strip,
            Self::DowngradePreset => GuardianActionKind::Downgrade,
        }
    }
}

impl From<GuardianDirective> for DirectiveCopyProjection {
    fn from(directive: GuardianDirective) -> Self {
        let kind = match directive {
            GuardianDirective::UseManagedJava { .. } => {
                RecoveryKindProjection::SwitchManagedRuntime
            }
            GuardianDirective::StripJvmArgs { .. } => RecoveryKindProjection::StripRawJvmArgs,
            GuardianDirective::DowngradeJvmPreset { .. } => RecoveryKindProjection::DowngradePreset,
            GuardianDirective::DisableCustomGc => RecoveryKindProjection::DisableCustomGc,
        };
        Self {
            kind,
            description: guardian_directive_description(&directive),
        }
    }
}

#[test]
fn checked_in_guardian_launch_copy_is_byte_stable_and_complete() {
    let fixture = committed_copy_fixture();
    assert_snapshot_coverage(&fixture);
    let replayed = replay_snapshot(&fixture);
    let first = snapshot_bytes(&replayed);
    let second = snapshot_bytes(&replay_snapshot(&fixture));

    assert_eq!(fixture, replayed);
    assert_eq!(first, second);
    assert_eq!(first.as_slice(), COPY_FIXTURE.as_bytes());
}

#[test]
fn guardian_launch_copy_fixture_rejects_unknown_and_malformed_fields() {
    let mut unknown =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("launch copy fixture JSON");
    unknown["cases"][0]["input"]["unexpected"] = serde_json::Value::Bool(true);
    let error = serde_json::from_value::<GuardianLaunchCopySnapshot>(unknown)
        .expect_err("unknown launch copy input field must be rejected");
    assert!(error.to_string().contains("unknown field"));

    let mut malformed =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("launch copy fixture JSON");
    let outcome = malformed["cases"]
        .as_array_mut()
        .expect("launch copy cases")
        .iter_mut()
        .find_map(|case| case.get_mut("output")?.get_mut("outcome"))
        .and_then(serde_json::Value::as_object_mut)
        .expect("launch copy outcome");
    outcome.remove("summary");
    let error = serde_json::from_value::<GuardianLaunchCopySnapshot>(malformed)
        .expect_err("malformed launch copy outcome must be rejected");
    assert!(error.to_string().contains("missing field `summary`"));

    let mut nested =
        serde_json::from_str::<serde_json::Value>(COPY_FIXTURE).expect("launch copy fixture JSON");
    let directive = nested["cases"]
        .as_array_mut()
        .expect("launch copy cases")
        .iter_mut()
        .find_map(|case| case.get_mut("output")?.get_mut("directive"))
        .and_then(serde_json::Value::as_object_mut)
        .expect("launch copy directive");
    directive.insert(
        "effect".to_string(),
        serde_json::Value::String("owned_by_k2".into()),
    );
    let error = serde_json::from_value::<GuardianLaunchCopySnapshot>(nested)
        .expect_err("unknown directive copy field must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_launch_copy_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian launch copy snapshot"
    );
    let committed = committed_copy_fixture();
    assert_snapshot_coverage(&committed);
    let replayed = replay_snapshot(&committed);
    assert_snapshot_coverage(&replayed);
    std::fs::write(snapshot_fixture_path(), snapshot_bytes(&replayed))
        .expect("write regenerated Guardian launch copy fixture");
}

fn committed_copy_fixture() -> GuardianLaunchCopySnapshot {
    serde_json::from_str(COPY_FIXTURE).expect("strict committed Guardian launch copy fixture")
}

fn replay_snapshot(snapshot: &GuardianLaunchCopySnapshot) -> GuardianLaunchCopySnapshot {
    let boundary_sources = boundary_source_map();
    GuardianLaunchCopySnapshot {
        schema: snapshot.schema,
        cases: snapshot
            .cases
            .iter()
            .map(|case| GuardianLaunchCopyCase {
                id: case.id.clone(),
                input: case.input.clone(),
                output: render_output(&case.input, &boundary_sources),
            })
            .collect(),
    }
}

fn render_output(
    input: &GuardianLaunchCopyInput,
    boundary_sources: &HashMap<String, LaunchBoundaryCopySource>,
) -> GuardianLaunchCopyOutput {
    match input {
        GuardianLaunchCopyInput::BoundaryReference { boundary_id } => {
            let source = boundary_sources
                .get(boundary_id)
                .unwrap_or_else(|| panic!("unknown launch boundary copy source {boundary_id}"));
            project_output(
                source.authored_decision,
                source.outcome.clone(),
                source.directive.clone(),
            )
        }
        GuardianLaunchCopyInput::ObservedCrash {
            failure_class,
            phase,
            suspected_mods,
        } => {
            let crash_evidence = crash_evidence(suspected_mods);
            project_output(
                None,
                author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
                    *failure_class,
                    crash_evidence.as_ref(),
                    (*phase).into(),
                )),
                None,
            )
        }
        GuardianLaunchCopyInput::PrepareFailure {
            mode,
            failure_class,
            public_error,
            requested_java_present,
            explicit_java_override_present,
            explicit_jvm_args_present,
            runtime_intervention_applied,
            raw_jvm_args_intervention_applied,
        } => project_launch_failure_output(guardian_prepare_failure_outcome(
            GuardianPrepareFailureRequest {
                mode: *mode,
                failure_class: *failure_class,
                public_error,
                requested_java_present: *requested_java_present,
                explicit_java_override_present: *explicit_java_override_present,
                explicit_jvm_args_present: *explicit_jvm_args_present,
                runtime_intervention_applied: *runtime_intervention_applied,
                raw_jvm_args_intervention_applied: *raw_jvm_args_intervention_applied,
            },
        )),
        GuardianLaunchCopyInput::StartupFailure {
            mode,
            observation,
            suspected_mods,
            target_version_id,
            runtime_major,
            requested_java_present,
            explicit_java_override_present,
            explicit_jvm_args_present,
            explicit_jvm_preset_present,
            startup_recovery_applied,
            disable_custom_gc,
            effective_preset,
        } => {
            let crash_evidence = crash_evidence(suspected_mods);
            project_launch_failure_output(guardian_startup_failure_outcome(
                GuardianStartupFailureRequest {
                    mode: *mode,
                    observation: (*observation).into(),
                    crash_evidence: crash_evidence.as_ref(),
                    integrity_facts: &[],
                    registered_artifact_repair_target: None,
                    target_version_id,
                    runtime_major: *runtime_major,
                    requested_java_present: *requested_java_present,
                    explicit_java_override_present: *explicit_java_override_present,
                    explicit_jvm_args_present: *explicit_jvm_args_present,
                    explicit_jvm_preset_present: *explicit_jvm_preset_present,
                    startup_recovery_applied: *startup_recovery_applied,
                    disable_custom_gc: *disable_custom_gc,
                    effective_preset,
                },
            ))
        }
        GuardianLaunchCopyInput::PresetAdjustment {
            mode,
            requested_preset,
            effective_preset,
            explicit_jvm_preset_present,
        } => {
            let request = GuardianPresetAdjustmentRequest {
                mode: *mode,
                requested_preset,
                effective_preset,
                explicit_jvm_preset_present: *explicit_jvm_preset_present,
            };
            let (_, decision, _) =
                preset_adjustment_snapshot(&request).expect("fixture preset adjustment coordinate");
            let directive = super::guardian_prelaunch_preset_adjustment_directive(request)
                .expect("fixture preset adjustment directive");
            project_output(Some(decision.kind()), None, Some(directive))
        }
        GuardianLaunchCopyInput::RecoverySuppressed { kind } => project_output(
            None,
            Some(
                author_guardian_copy(GuardianCopyRequest::launch_recovery_suppressed(
                    &launch_recovery_plan(*kind).directive,
                ))
                .expect("launch recovery suppression copy"),
            ),
            None,
        ),
    }
}

fn project_launch_failure_output(
    outcome: super::GuardianLaunchFailureOutcome,
) -> GuardianLaunchCopyOutput {
    project_output(
        Some(outcome.guardian_decision.kind()),
        Some(outcome.user_outcome),
        outcome.directive,
    )
}

fn project_output(
    authored_decision: Option<GuardianActionKind>,
    outcome: Option<GuardianUserOutcome>,
    directive: Option<GuardianDirective>,
) -> GuardianLaunchCopyOutput {
    GuardianLaunchCopyOutput {
        authored_decision,
        outcome: outcome.map(Into::into),
        directive: directive.map(Into::into),
    }
}

fn boundary_source_map() -> HashMap<String, LaunchBoundaryCopySource> {
    launch_boundary_copy_sources()
        .into_iter()
        .map(|source| (source.id.clone(), source))
        .collect()
}

fn crash_evidence(suspected_mods: &[String]) -> Option<CrashEvidence> {
    if suspected_mods.is_empty() {
        return None;
    }
    let suspected_mods = suspected_mods
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect::<Vec<_>>();
    Some(
        serde_json::from_value(serde_json::json!({
            "source": "minecraft_crash_report",
            "truncated": false,
            "suspected_mods": suspected_mods,
            "names_out_of_memory": false
        }))
        .expect("typed launch copy crash evidence"),
    )
}

fn launch_recovery_plan(kind: RecoveryKindProjection) -> super::GuardianLaunchRecoveryPlan {
    let (directive, failure_class) = match kind {
        RecoveryKindProjection::SwitchManagedRuntime => (
            GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::StartupRecovery,
            },
            LaunchFailureClass::JavaRuntimeMismatch,
        ),
        RecoveryKindProjection::StripRawJvmArgs => (
            GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            },
            LaunchFailureClass::JvmUnsupportedOption,
        ),
        RecoveryKindProjection::DowngradePreset => (
            GuardianDirective::startup_preset_downgrade("performance"),
            LaunchFailureClass::JvmUnsupportedOption,
        ),
        RecoveryKindProjection::DisableCustomGc => (
            GuardianDirective::DisableCustomGc,
            LaunchFailureClass::JvmUnsupportedOption,
        ),
    };
    plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
        instance_id: "launch-copy-snapshot-instance",
        mode: GuardianMode::Managed,
        directive,
        failure_class,
        user_intent_hash:
            "sha256.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa",
    })
    .expect("valid launch copy recovery plan")
}

fn assert_snapshot_coverage(snapshot: &GuardianLaunchCopySnapshot) {
    assert_eq!(snapshot.schema, SnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), EXPECTED_CASE_COUNT);
    let ids = snapshot
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        ids.iter().copied().collect::<HashSet<_>>().len(),
        EXPECTED_CASE_COUNT
    );
    assert!(
        snapshot
            .cases
            .iter()
            .all(|case| case.id == canonical_case_id(&case.input))
    );

    let boundary_ids = snapshot
        .cases
        .iter()
        .filter_map(|case| match &case.input {
            GuardianLaunchCopyInput::BoundaryReference { boundary_id } => Some(boundary_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(boundary_ids.len(), BOUNDARY_CASE_COUNT);
    assert_eq!(boundary_ids, committed_launch_boundary_case_ids());

    let observed = snapshot
        .cases
        .iter()
        .filter(|case| matches!(&case.input, GuardianLaunchCopyInput::ObservedCrash { .. }))
        .collect::<Vec<_>>();
    assert_eq!(observed.len(), OBSERVED_CASE_COUNT);
    for failure_class in ACCEPTED_CRASH_CLASSES {
        for phase in [
            ObservedPhaseInput::BeforeBoot,
            ObservedPhaseInput::AfterBoot,
        ] {
            assert!(snapshot.cases.iter().any(|case| {
                matches!(
                    &case.input,
                    GuardianLaunchCopyInput::ObservedCrash {
                        failure_class: actual,
                        phase: actual_phase,
                        suspected_mods,
                    } if *actual == failure_class && *actual_phase == phase && suspected_mods.is_empty()
                )
            }));
        }
    }
    let supplemental_ids = snapshot
        .cases
        .iter()
        .filter(|case| {
            !matches!(
                &case.input,
                GuardianLaunchCopyInput::BoundaryReference { .. }
            )
        })
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(supplemental_ids, EXPECTED_SUPPLEMENTAL_IDS);
    let rejected_observed = snapshot
        .cases
        .iter()
        .find(|case| case.id == "observed.unknown.after_boot.generic")
        .expect("unsupported observed crash class case");
    assert!(rejected_observed.output.outcome.is_none());
    assert!(rejected_observed.output.directive.is_none());

    let mut axis_counts = [0_usize; 6];
    for case in &snapshot.cases {
        let axis = match &case.input {
            GuardianLaunchCopyInput::BoundaryReference { .. } => 0,
            GuardianLaunchCopyInput::ObservedCrash { .. } => 1,
            GuardianLaunchCopyInput::PrepareFailure { .. } => 2,
            GuardianLaunchCopyInput::StartupFailure { .. } => 3,
            GuardianLaunchCopyInput::PresetAdjustment { .. } => 4,
            GuardianLaunchCopyInput::RecoverySuppressed { .. } => 5,
        };
        axis_counts[axis] += 1;
        assert_exact_supplemental_input(case);
        assert_output_bounds_and_relationships(case);
    }
    assert_eq!(axis_counts, [55, 13, 3, 4, 1, 4]);

    assert_observed_startup_relationships(snapshot);
    assert_directive_description_coverage(snapshot);
    assert_public_fixture(snapshot);
}

fn assert_exact_supplemental_input(case: &GuardianLaunchCopyCase) {
    const HOSTILE_PUBLIC_ERROR: &str =
        "/home/alice/.jdks/java.exe --accessToken secret -Xmx8192M --username Alice";
    let expected_mods = [
        "Example Machines".to_string(),
        "Ignored Safe Mod".to_string(),
    ];
    match &case.input {
        GuardianLaunchCopyInput::BoundaryReference { .. }
        | GuardianLaunchCopyInput::RecoverySuppressed { .. } => {}
        GuardianLaunchCopyInput::ObservedCrash {
            failure_class,
            suspected_mods,
            ..
        } => {
            assert!(
                suspected_mods.is_empty()
                    || (*failure_class == LaunchFailureClass::ModAttributedCrash
                        && suspected_mods == &expected_mods)
            );
        }
        GuardianLaunchCopyInput::PrepareFailure {
            mode,
            failure_class,
            public_error,
            requested_java_present,
            explicit_java_override_present,
            explicit_jvm_args_present,
            runtime_intervention_applied,
            raw_jvm_args_intervention_applied,
        } => {
            assert_eq!(*mode, GuardianMode::Managed);
            assert_eq!(public_error, HOSTILE_PUBLIC_ERROR);
            assert!(*runtime_intervention_applied);
            assert!(*raw_jvm_args_intervention_applied);
            let expected = match failure_class {
                LaunchFailureClass::Unknown => (true, true, true),
                LaunchFailureClass::JavaRuntimeMismatch => (true, true, false),
                LaunchFailureClass::JvmUnsupportedOption => (false, false, true),
                _ => panic!("unexpected supplemental prepare failure class"),
            };
            assert_eq!(
                (
                    *requested_java_present,
                    *explicit_java_override_present,
                    *explicit_jvm_args_present,
                ),
                expected
            );
        }
        GuardianLaunchCopyInput::StartupFailure {
            mode,
            observation,
            suspected_mods,
            target_version_id,
            runtime_major,
            requested_java_present,
            explicit_java_override_present,
            explicit_jvm_args_present,
            explicit_jvm_preset_present,
            startup_recovery_applied,
            disable_custom_gc,
            effective_preset,
        } => {
            assert_eq!(*mode, GuardianMode::Managed);
            assert!(!*requested_java_present);
            assert!(!*explicit_jvm_args_present);
            assert!(!*explicit_jvm_preset_present);
            assert!(!*startup_recovery_applied);
            assert!(*disable_custom_gc);
            match observation {
                StartupObservationInput::Exited {
                    failure_class: LaunchFailureClass::ModAttributedCrash,
                } => {
                    assert_eq!(suspected_mods, &expected_mods);
                    assert_eq!(target_version_id, "1.21.1");
                    assert_eq!(*runtime_major, 21);
                    assert!(!*explicit_java_override_present);
                    assert!(effective_preset.is_empty());
                }
                StartupObservationInput::Exited {
                    failure_class: LaunchFailureClass::Unknown,
                }
                | StartupObservationInput::Stalled => {
                    assert!(suspected_mods.is_empty());
                    assert_eq!(target_version_id, "1.21.1");
                    assert_eq!(*runtime_major, 21);
                    assert!(*explicit_java_override_present);
                    assert!(effective_preset.is_empty());
                }
                StartupObservationInput::Exited {
                    failure_class: LaunchFailureClass::JvmUnsupportedOption,
                } => {
                    assert!(suspected_mods.is_empty());
                    assert_eq!(target_version_id, "1.12.2");
                    assert_eq!(*runtime_major, 17);
                    assert!(!*explicit_java_override_present);
                    assert_eq!(effective_preset, "ultra_low_latency");
                }
                _ => panic!("unexpected supplemental startup observation"),
            }
        }
        GuardianLaunchCopyInput::PresetAdjustment {
            mode,
            requested_preset,
            effective_preset,
            explicit_jvm_preset_present,
        } => {
            assert_eq!(*mode, GuardianMode::Managed);
            assert_eq!(
                requested_preset,
                &format!("requested_preset_{}", "x".repeat(96))
            );
            assert_eq!(effective_preset, "performance-beta");
            assert!(*explicit_jvm_preset_present);
        }
    }
}

fn assert_output_bounds_and_relationships(case: &GuardianLaunchCopyCase) {
    let output = &case.output;
    if let (Some(authored), Some(outcome)) = (output.authored_decision, &output.outcome) {
        assert_eq!(outcome.decision, authored, "outcome drift for {}", case.id);
    }
    if let Some(outcome) = &output.outcome {
        assert!(!outcome.summary.is_empty());
        assert_eq!(outcome.summary, outcome.summary.trim());
        assert!(outcome.summary.len() <= 180);
        assert!(outcome.details.len() <= 6);
        assert!(outcome.guidance.len() <= 6);
        assert!(
            outcome
                .details
                .iter()
                .chain(&outcome.guidance)
                .all(|line| !line.is_empty() && line == line.trim() && line.len() <= 240)
        );
        assert_eq!(
            outcome.details.iter().collect::<HashSet<_>>().len(),
            outcome.details.len()
        );
        assert_eq!(
            outcome.guidance.iter().collect::<HashSet<_>>().len(),
            outcome.guidance.len()
        );
    }
    if let Some(directive) = &output.directive {
        assert!(!directive.description.is_empty());
        assert_eq!(directive.description, directive.description.trim());
        assert!(directive.description.len() <= 240);
        assert_eq!(
            output.authored_decision,
            Some(directive.kind.action_kind()),
            "directive decision drift for {}",
            case.id
        );
        if let Some(outcome) = &output.outcome {
            assert_eq!(outcome.details.first(), Some(&directive.description));
        }
    }
}

fn assert_observed_startup_relationships(snapshot: &GuardianLaunchCopySnapshot) {
    for failure_class in ACCEPTED_CRASH_CLASSES {
        let boundary_id = format!(
            "boundary.startup--exited-{}--managed-default",
            failure_class.as_str()
        );
        let observed_id = format!("observed.{}.before_boot.generic", failure_class.as_str());
        let boundary = outcome_for(snapshot, &boundary_id);
        let observed = outcome_for(snapshot, &observed_id);
        assert_eq!(boundary.decision, GuardianActionKind::Block);
        assert_eq!(observed.decision, GuardianActionKind::Block);
        assert_eq!(boundary.phase, OperationPhase::Launching);
        assert_eq!(observed.phase, OperationPhase::Launching);
        assert_eq!(boundary.summary, observed.summary);
        assert_eq!(boundary.details, observed.details);
        assert_eq!(boundary.guidance, observed.guidance);
    }

    let startup = outcome_for(snapshot, "startup.mod_attributed_crash.public_safe_first");
    let observed = outcome_for(
        snapshot,
        "observed.mod_attributed_crash.before_boot.public_safe_first",
    );
    assert_eq!(startup.details, observed.details);
    assert_eq!(startup.guidance, observed.guidance);
}

fn assert_directive_description_coverage(snapshot: &GuardianLaunchCopySnapshot) {
    let boundary_directives = snapshot
        .cases
        .iter()
        .filter(|case| {
            matches!(
                &case.input,
                GuardianLaunchCopyInput::BoundaryReference { .. }
            )
        })
        .filter(|case| case.output.directive.is_some())
        .count();
    assert_eq!(boundary_directives, 9);
    let descriptions = snapshot
        .cases
        .iter()
        .filter_map(|case| case.output.directive.as_ref())
        .map(|directive| directive.description.as_str())
        .collect::<HashSet<_>>();
    assert!(descriptions.len() >= 7);
}

fn assert_public_fixture(snapshot: &GuardianLaunchCopySnapshot) {
    let lower = serde_json::to_string(
        &snapshot
            .cases
            .iter()
            .map(|case| &case.output)
            .collect::<Vec<_>>(),
    )
    .expect("launch copy output projections")
    .to_ascii_lowercase();
    for excluded in [
        "/home",
        "alice",
        "java.exe",
        "accesstoken",
        "-xmx",
        "--username",
        "secret",
        "ignored safe mod",
        "fixture-only launch recovery directive",
    ] {
        assert!(!lower.contains(excluded), "fixture contains {excluded}");
    }
}

fn outcome_for<'a>(
    snapshot: &'a GuardianLaunchCopySnapshot,
    id: &str,
) -> &'a GuardianUserOutcomeProjection {
    snapshot
        .cases
        .iter()
        .find(|case| case.id == id)
        .and_then(|case| case.output.outcome.as_ref())
        .unwrap_or_else(|| panic!("missing launch copy outcome {id}"))
}

fn canonical_case_id(input: &GuardianLaunchCopyInput) -> String {
    match input {
        GuardianLaunchCopyInput::BoundaryReference { boundary_id } => {
            format!("boundary.{boundary_id}")
        }
        GuardianLaunchCopyInput::ObservedCrash {
            failure_class,
            phase,
            suspected_mods,
        } => format!(
            "observed.{}.{}.{}",
            failure_class.as_str(),
            observed_phase_id(*phase),
            if suspected_mods.is_empty() {
                "generic"
            } else {
                "public_safe_first"
            }
        ),
        GuardianLaunchCopyInput::PrepareFailure { failure_class, .. } => match failure_class {
            LaunchFailureClass::Unknown => "prepare.public_error.hostile_unknown".to_string(),
            LaunchFailureClass::JavaRuntimeMismatch => {
                "prepare.public_error.rejected_java_runtime_mismatch".to_string()
            }
            LaunchFailureClass::JvmUnsupportedOption => {
                "prepare.public_error.rejected_jvm_unsupported_option".to_string()
            }
            _ => panic!("unsupported supplemental prepare failure class"),
        },
        GuardianLaunchCopyInput::StartupFailure {
            observation,
            suspected_mods,
            target_version_id,
            ..
        } => match observation {
            StartupObservationInput::Exited {
                failure_class: LaunchFailureClass::ModAttributedCrash,
            } if !suspected_mods.is_empty() => {
                "startup.mod_attributed_crash.public_safe_first".to_string()
            }
            StartupObservationInput::Exited {
                failure_class: LaunchFailureClass::Unknown,
            } => "startup.guidance.generic_explicit_override".to_string(),
            StartupObservationInput::Stalled => {
                "startup.guidance.stalled_explicit_override".to_string()
            }
            StartupObservationInput::Exited {
                failure_class: LaunchFailureClass::JvmUnsupportedOption,
            } if target_version_id == "1.12.2" => {
                "startup.jvm_unsupported_option.legacy_preset_downgrade".to_string()
            }
            _ => panic!("unsupported supplemental startup failure coordinate"),
        },
        GuardianLaunchCopyInput::PresetAdjustment { .. } => "preset.bounded_labels".to_string(),
        GuardianLaunchCopyInput::RecoverySuppressed { kind } => {
            format!("recovery_suppressed.{}", recovery_kind_id(*kind))
        }
    }
}

fn observed_phase_id(phase: ObservedPhaseInput) -> &'static str {
    match phase {
        ObservedPhaseInput::BeforeBoot => "before_boot",
        ObservedPhaseInput::AfterBoot => "after_boot",
    }
}

fn recovery_kind_id(kind: RecoveryKindProjection) -> &'static str {
    match kind {
        RecoveryKindProjection::SwitchManagedRuntime => "managed_runtime",
        RecoveryKindProjection::StripRawJvmArgs => "strip_jvm_args",
        RecoveryKindProjection::DowngradePreset => "downgrade_preset",
        RecoveryKindProjection::DisableCustomGc => "disable_custom_gc",
    }
}

fn snapshot_bytes(snapshot: &GuardianLaunchCopySnapshot) -> Vec<u8> {
    let pretty = serde_json::to_string_pretty(snapshot).expect("serialize launch copy snapshot");
    format!("{pretty}\n").into_bytes()
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-launch-copy-v1.json")
}
