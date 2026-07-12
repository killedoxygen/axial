use super::preflight_decision_snapshot::{
    committed_preflight_boundary_case_ids, replay_committed_preflight_boundary_case,
};
use super::{
    FactReliability, GuardianActionKind, GuardianConfidence, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianMode, GuardianPreflightOutcome, GuardianPreflightOutcomeRequest,
    GuardianPreflightReadiness, GuardianPreflightResourceSignals, GuardianSeverity,
    guardian_preflight_outcome,
};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const COPY_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-preflight-copy-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_PREFLIGHT_COPY_SNAPSHOT";
const CASE_COUNT: usize = 57;
const BOUNDARY_CASE_COUNT: usize = 35;
const STATIC_CASE_COUNT: usize = 17;
const HISTORICAL_CASE_COUNT: usize = 5;

const EXPECTED_CASE_IDS: [&str; CASE_COUNT] = [
    "boundary--fallback--java_major_mismatch",
    "boundary--fallback--java_override_missing",
    "boundary--fallback--java_override_undefined_sentinel",
    "boundary--fallback--java_probe_failed",
    "boundary--fallback--java_update_too_old",
    "boundary--precedence--client-jar-missing-block",
    "boundary--precedence--disabled-jvm-args-block",
    "boundary--precedence--empty-record-only-permitted",
    "boundary--precedence--java-override-custom-ask-user",
    "boundary--precedence--jvm-args-custom-warning-over-ask-user",
    "boundary--precedence--managed-runtime-missing-readiness-block-over-record-only",
    "boundary--precedence--readiness-flag-block",
    "boundary--precedence--readiness-over-strip",
    "boundary--precedence--strip-over-resource-warning",
    "boundary--strip--jvm_arg_agent_override",
    "boundary--strip--jvm_arg_memory_conflict",
    "boundary--strip--jvm_arg_reserved_launcher_flag",
    "boundary--strip--jvm_arg_unlock_order_invalid",
    "boundary--strip--jvm_arg_unsafe_classpath_override",
    "boundary--strip--jvm_arg_unsafe_native_path_override",
    "boundary--strip--jvm_arg_unsupported_gc",
    "boundary--strip--jvm_args_parse_failed",
    "boundary--warning--custom_java_override_present",
    "boundary--warning--custom_jvm_args_present",
    "boundary--warning--custom_jvm_preset_present",
    "boundary--warning--java_override_empty",
    "boundary--warning--launch_memory_allocation_low",
    "boundary--warning--launch_memory_min_clamped",
    "boundary--warning--launch_resource_cpu_pressure",
    "boundary--warning--launch_resource_disk_pressure",
    "boundary--warning--launch_resource_install_pressure",
    "boundary--warning--launch_resource_memory_pressure",
    "boundary--warning--recent_repair_failed",
    "boundary--warning--recent_startup_failure",
    "boundary--warning--repair_suppressed_until",
    "history--occurrence-windows",
    "history--oom-headroom",
    "history--repair-variants",
    "history--saturated-warning-composition",
    "history--suppression-offset",
    "static--artifact-corrupt-block",
    "static--asset-index-missing-block",
    "static--install-incomplete-block",
    "static--installed-version-metadata-missing-block",
    "static--java-major-mismatch-block",
    "static--java-override-unavailable-block",
    "static--java-probe-failed-block",
    "static--java-update-too-old-block",
    "static--jvm-unsafe-override-block",
    "static--jvm-unsafe-override-warn",
    "static--jvm-unsupported-block",
    "static--jvm-unsupported-warn",
    "static--launcher-signature-corrupt-block",
    "static--libraries-missing-block",
    "static--managed-runtime-corrupt-repair",
    "static--managed-runtime-missing-record-only",
    "static--parent-version-metadata-missing-block",
];

const EXPECTED_STATIC_COORDINATES: [StaticCoordinate; STATIC_CASE_COUNT] = [
    StaticCoordinate::new(
        "static--artifact-corrupt-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::ArtifactChecksumMismatch,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--asset-index-missing-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::AssetIndexMissing,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--install-incomplete-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::IncompleteInstall,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--installed-version-metadata-missing-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::VersionJsonMissing,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--java-major-mismatch-block",
        GuardianMode::Custom,
        true,
        GuardianFactId::JavaMajorMismatch,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--java-override-unavailable-block",
        GuardianMode::Custom,
        false,
        GuardianFactId::JavaOverrideMissing,
        StaticFactSource::Readiness,
        true,
    ),
    StaticCoordinate::new(
        "static--java-probe-failed-block",
        GuardianMode::Custom,
        true,
        GuardianFactId::JavaProbeFailed,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--java-update-too-old-block",
        GuardianMode::Custom,
        true,
        GuardianFactId::JavaUpdateTooOld,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--jvm-unsafe-override-block",
        GuardianMode::Disabled,
        true,
        GuardianFactId::JvmArgReservedLauncherFlag,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--jvm-unsafe-override-warn",
        GuardianMode::Custom,
        true,
        GuardianFactId::JvmArgReservedLauncherFlag,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--jvm-unsupported-block",
        GuardianMode::Disabled,
        true,
        GuardianFactId::JvmArgUnsupportedGc,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--jvm-unsupported-warn",
        GuardianMode::Custom,
        true,
        GuardianFactId::JvmArgUnsupportedGc,
        StaticFactSource::Direct,
        true,
    ),
    StaticCoordinate::new(
        "static--launcher-signature-corrupt-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::LauncherManagedArtifactSignatureCorruption,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--libraries-missing-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::LibrariesMissing,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--managed-runtime-corrupt-repair",
        GuardianMode::Managed,
        true,
        GuardianFactId::ManagedRuntimeCorrupt,
        StaticFactSource::Direct,
        false,
    ),
    StaticCoordinate::new(
        "static--managed-runtime-missing-record-only",
        GuardianMode::Managed,
        true,
        GuardianFactId::ManagedRuntimeMissing,
        StaticFactSource::Readiness,
        false,
    ),
    StaticCoordinate::new(
        "static--parent-version-metadata-missing-block",
        GuardianMode::Managed,
        false,
        GuardianFactId::ParentVersionMissing,
        StaticFactSource::Readiness,
        false,
    ),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum CopySnapshotSchema {
    #[serde(rename = "axial.guardian.preflight_copy.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCopySnapshot {
    schema: CopySnapshotSchema,
    cases: Vec<PreflightCopyCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCopyCase {
    id: String,
    axis: CopyAxis,
    input: PreflightCopyInput,
    output: PreflightCopyOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CopyAxis {
    BoundaryRef,
    Static,
    HistoricalComposition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PreflightCopyInput {
    BoundaryRef {
        boundary_ref: String,
        historical_overrides: Vec<BoundaryHistoricalOverride>,
    },
    Static {
        mode: GuardianMode,
        launchable: bool,
        fact_id: GuardianFactId,
        source: StaticFactSource,
        explicit_user_intent: bool,
    },
    HistoricalComposition {
        mode: GuardianMode,
        facts: Vec<HistoricalFactInput>,
        resources: Vec<CopyResourceSignal>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BoundaryHistoricalOverride {
    RecentStartupFailure {
        failure_class: HistoricalCrashClass,
        occurrences: Option<u32>,
        latest_observed_today: Option<bool>,
        occurrences_today: Option<u32>,
        current_memory_mb: Option<u32>,
        suggested_memory_mb: Option<u32>,
    },
    RecentRepairFailed {
        recovery: HistoricalRecovery,
    },
    RepairSuppressedUntil {
        suppression_until: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StaticFactSource {
    Direct,
    Readiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StaticCoordinate {
    id: &'static str,
    mode: GuardianMode,
    launchable: bool,
    fact_id: GuardianFactId,
    source: StaticFactSource,
    explicit_user_intent: bool,
}

impl StaticCoordinate {
    const fn new(
        id: &'static str,
        mode: GuardianMode,
        launchable: bool,
        fact_id: GuardianFactId,
        source: StaticFactSource,
        explicit_user_intent: bool,
    ) -> Self {
        Self {
            id,
            mode,
            launchable,
            fact_id,
            source,
            explicit_user_intent,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum HistoricalFactInput {
    StartupFailure {
        target: String,
        failure_class: HistoricalCrashClass,
        occurrences: Option<u32>,
        latest_observed_today: Option<bool>,
        occurrences_today: Option<u32>,
        current_memory_mb: Option<u32>,
        suggested_memory_mb: Option<u32>,
    },
    RepairFailed {
        target: String,
        recovery: HistoricalRecovery,
    },
    Suppressed {
        target: String,
        suppression_until: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoricalCrashClass {
    OutOfMemory,
    GraphicsDriverCrash,
    MissingDependency,
    ModTransformationFailure,
    ModAttributedCrash,
}

impl HistoricalCrashClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::OutOfMemory => "out_of_memory",
            Self::GraphicsDriverCrash => "graphics_driver_crash",
            Self::MissingDependency => "missing_dependency",
            Self::ModTransformationFailure => "mod_transformation_failure",
            Self::ModAttributedCrash => "mod_attributed_crash",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoricalRecovery {
    JavaRuntime,
    JvmArgs,
    JvmPreset,
}

impl HistoricalRecovery {
    const fn as_str(self) -> &'static str {
        match self {
            Self::JavaRuntime => "java_runtime_recovery",
            Self::JvmArgs => "jvm_arg_unsupported",
            Self::JvmPreset => "jvm_preset_recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CopyResourceSignal {
    MemoryClamped,
    LowMemoryAllocation,
    MemoryPressure,
    CpuPressure,
    InstallPressure,
    DiskPressure,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCopyOutput {
    kernel_decision: GuardianActionKind,
    effective_decision: GuardianActionKind,
    phase: OperationPhase,
    summary: String,
    details: Vec<String>,
    guidance: Vec<String>,
}

#[test]
fn checked_in_guardian_preflight_copy_is_byte_stable_and_complete() {
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
fn guardian_preflight_copy_snapshot_rejects_unknown_nested_fields() {
    let value = serde_json::from_str::<serde_json::Value>(COPY_FIXTURE)
        .expect("preflight copy fixture JSON");
    let mut input = value.clone();
    input["cases"][0]["input"]["unexpected"] = serde_json::Value::Bool(true);
    assert_unknown_field_rejected(input);

    let mut output = value.clone();
    output["cases"][0]["output"]["unexpected"] = serde_json::Value::Bool(true);
    assert_unknown_field_rejected(output);

    let mut case = value;
    case["cases"][0]["unexpected"] = serde_json::Value::Bool(true);
    assert_unknown_field_rejected(case);
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_preflight_copy_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian preflight copy snapshot"
    );
    let fixture = committed_copy_fixture();
    assert_snapshot_coverage(&fixture);
    let replayed = replay_snapshot(&fixture);
    assert_snapshot_coverage(&replayed);
    std::fs::write(snapshot_fixture_path(), snapshot_bytes(&replayed))
        .expect("write regenerated Guardian preflight copy snapshot");
}

fn committed_copy_fixture() -> PreflightCopySnapshot {
    serde_json::from_str(COPY_FIXTURE).expect("strict committed Guardian preflight copy fixture")
}

fn assert_unknown_field_rejected(value: serde_json::Value) {
    let error = serde_json::from_value::<PreflightCopySnapshot>(value)
        .expect_err("nested preflight copy fixture fields must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

fn replay_snapshot(snapshot: &PreflightCopySnapshot) -> PreflightCopySnapshot {
    PreflightCopySnapshot {
        schema: snapshot.schema,
        cases: snapshot
            .cases
            .iter()
            .map(|case| {
                let outcome = replay_input(&case.input);
                assert_safety_relation(&outcome);
                PreflightCopyCase {
                    id: case.id.clone(),
                    axis: case.axis,
                    input: case.input.clone(),
                    output: output_projection(outcome),
                }
            })
            .collect(),
    }
}

fn replay_input(input: &PreflightCopyInput) -> GuardianPreflightOutcome {
    match input {
        PreflightCopyInput::BoundaryRef {
            boundary_ref,
            historical_overrides,
        } => {
            let field_overrides = historical_overrides
                .iter()
                .map(boundary_historical_override)
                .collect::<Vec<_>>();
            replay_committed_preflight_boundary_case(boundary_ref, &field_overrides).unwrap_or_else(
                || panic!("unknown committed preflight boundary case: {boundary_ref}"),
            )
        }
        PreflightCopyInput::Static {
            mode,
            launchable,
            fact_id,
            source,
            explicit_user_intent,
        } => replay_static_input(*mode, *launchable, *fact_id, *source, *explicit_user_intent),
        PreflightCopyInput::HistoricalComposition {
            mode,
            facts,
            resources,
        } => replay_historical_composition(*mode, facts, resources),
    }
}

fn replay_static_input(
    mode: GuardianMode,
    launchable: bool,
    fact_id: GuardianFactId,
    source: StaticFactSource,
    explicit_user_intent: bool,
) -> GuardianPreflightOutcome {
    let fact = static_fact(fact_id);
    let (facts, readiness_facts) = match source {
        StaticFactSource::Direct => (std::slice::from_ref(&fact), &[][..]),
        StaticFactSource::Readiness => (&[][..], std::slice::from_ref(&fact)),
    };
    guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
        operation_id: None,
        mode,
        phase: OperationPhase::Validating,
        facts,
        readiness: GuardianPreflightReadiness::from_facts(launchable, readiness_facts),
        resources: GuardianPreflightResourceSignals::default(),
        overrides: Default::default(),
        explicit_user_intent,
    })
}

fn replay_historical_composition(
    mode: GuardianMode,
    historical: &[HistoricalFactInput],
    resources: &[CopyResourceSignal],
) -> GuardianPreflightOutcome {
    let facts = historical.iter().map(historical_fact).collect::<Vec<_>>();
    guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
        resources: resource_signals(resources),
        ..GuardianPreflightOutcomeRequest::new(mode, &facts)
    })
}

fn output_projection(outcome: GuardianPreflightOutcome) -> PreflightCopyOutput {
    PreflightCopyOutput {
        kernel_decision: outcome.guardian_decision.kind,
        effective_decision: outcome.user_outcome.decision(),
        phase: outcome.user_outcome.phase(),
        summary: outcome.user_outcome.summary().to_string(),
        details: outcome.user_outcome.details().to_vec(),
        guidance: outcome.user_outcome.guidance().to_vec(),
    }
}

fn assert_safety_relation(outcome: &GuardianPreflightOutcome) {
    assert_eq!(outcome.safety.decision, outcome.user_outcome.decision());
    assert_eq!(outcome.safety.summary, outcome.user_outcome.summary());
    assert_eq!(
        outcome.safety.detail,
        outcome.user_outcome.details().first().cloned()
    );
}

fn static_fact(id: GuardianFactId) -> GuardianFact {
    let (domain, reliability, severity, ownership, target_kind) = match id {
        GuardianFactId::JavaOverrideMissing
        | GuardianFactId::JavaProbeFailed
        | GuardianFactId::JavaMajorMismatch
        | GuardianFactId::JavaUpdateTooOld => (
            GuardianDomain::Runtime,
            FactReliability::DirectStructured,
            GuardianSeverity::Blocking,
            OwnershipClass::UserOwned,
            TargetKind::Config,
        ),
        GuardianFactId::JvmArgReservedLauncherFlag | GuardianFactId::JvmArgUnsupportedGc => (
            GuardianDomain::Jvm,
            FactReliability::DirectStructured,
            GuardianSeverity::Blocking,
            OwnershipClass::UserOwned,
            TargetKind::Config,
        ),
        GuardianFactId::VersionJsonMissing
        | GuardianFactId::ParentVersionMissing
        | GuardianFactId::IncompleteInstall
        | GuardianFactId::AssetIndexMissing
        | GuardianFactId::LibrariesMissing => (
            GuardianDomain::Install,
            FactReliability::ExpectedMarkerAbsence,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Artifact,
        ),
        GuardianFactId::ArtifactChecksumMismatch => (
            GuardianDomain::Download,
            FactReliability::ExactClassifier,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Artifact,
        ),
        GuardianFactId::LauncherManagedArtifactSignatureCorruption => (
            GuardianDomain::Download,
            FactReliability::ExactClassifier,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Artifact,
        ),
        GuardianFactId::ManagedRuntimeMissing => (
            GuardianDomain::Runtime,
            FactReliability::ExpectedMarkerAbsence,
            GuardianSeverity::Recoverable,
            OwnershipClass::LauncherManaged,
            TargetKind::Runtime,
        ),
        GuardianFactId::ManagedRuntimeCorrupt => (
            GuardianDomain::Runtime,
            FactReliability::ExactClassifier,
            GuardianSeverity::Repairable,
            OwnershipClass::LauncherManaged,
            TargetKind::Runtime,
        ),
        _ => panic!("unsupported preflight copy static fact: {}", id.as_str()),
    };
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase: OperationPhase::Validating,
        reliability,
        severity: Some(severity),
        confidence: Some(GuardianConfidence::Confirmed),
        ownership,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            target_kind,
            "preflight_copy_snapshot",
            ownership,
        )),
        fields: Vec::new(),
    }
}

fn historical_fact(input: &HistoricalFactInput) -> GuardianFact {
    match input {
        HistoricalFactInput::StartupFailure {
            target,
            failure_class,
            occurrences,
            latest_observed_today,
            occurrences_today,
            current_memory_mb,
            suggested_memory_mb,
        } => {
            let mut fields = vec![public_field("failure_class", failure_class.as_str())];
            push_optional_field(&mut fields, "occurrences", *occurrences);
            if let Some(value) = latest_observed_today {
                fields.push(public_field("latest_observed_today", value.to_string()));
            }
            push_optional_field(&mut fields, "occurrences_today", *occurrences_today);
            push_optional_field(&mut fields, "current_memory_mb", *current_memory_mb);
            push_optional_field(&mut fields, "suggested_memory_mb", *suggested_memory_mb);
            historical_guardian_fact(
                GuardianFactId::RecentStartupFailure,
                GuardianDomain::Startup,
                OwnershipClass::UserOwned,
                target,
                fields,
            )
        }
        HistoricalFactInput::RepairFailed { target, recovery } => historical_guardian_fact(
            GuardianFactId::RecentRepairFailed,
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            target,
            vec![public_field("diagnosis", recovery.as_str())],
        ),
        HistoricalFactInput::Suppressed {
            target,
            suppression_until,
        } => historical_guardian_fact(
            GuardianFactId::RepairSuppressedUntil,
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            target,
            vec![public_field("suppression_until", suppression_until)],
        ),
    }
}

fn boundary_historical_override(
    input: &BoundaryHistoricalOverride,
) -> (GuardianFactId, Vec<EvidenceField>) {
    match input {
        BoundaryHistoricalOverride::RecentStartupFailure {
            failure_class,
            occurrences,
            latest_observed_today,
            occurrences_today,
            current_memory_mb,
            suggested_memory_mb,
        } => {
            let mut fields = vec![public_field("failure_class", failure_class.as_str())];
            push_optional_field(&mut fields, "occurrences", *occurrences);
            if let Some(value) = latest_observed_today {
                fields.push(public_field("latest_observed_today", value.to_string()));
            }
            push_optional_field(&mut fields, "occurrences_today", *occurrences_today);
            push_optional_field(&mut fields, "current_memory_mb", *current_memory_mb);
            push_optional_field(&mut fields, "suggested_memory_mb", *suggested_memory_mb);
            (GuardianFactId::RecentStartupFailure, fields)
        }
        BoundaryHistoricalOverride::RecentRepairFailed { recovery } => (
            GuardianFactId::RecentRepairFailed,
            vec![public_field("diagnosis", recovery.as_str())],
        ),
        BoundaryHistoricalOverride::RepairSuppressedUntil { suppression_until } => (
            GuardianFactId::RepairSuppressedUntil,
            vec![public_field("suppression_until", suppression_until)],
        ),
    }
}

fn historical_guardian_fact(
    id: GuardianFactId,
    domain: GuardianDomain,
    ownership: OwnershipClass,
    target: &str,
    fields: Vec<EvidenceField>,
) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase: OperationPhase::Validating,
        reliability: FactReliability::DirectStructured,
        severity: Some(GuardianSeverity::Warning),
        confidence: Some(GuardianConfidence::High),
        ownership,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::State,
            TargetKind::Instance,
            target,
            ownership,
        )),
        fields,
    }
}

fn public_field(key: &str, value: impl Into<String>) -> EvidenceField {
    EvidenceField::new(key, value, EvidenceSensitivity::Public)
}

fn push_optional_field(fields: &mut Vec<EvidenceField>, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        fields.push(public_field(key, value.to_string()));
    }
}

fn resource_signals(signals: &[CopyResourceSignal]) -> GuardianPreflightResourceSignals {
    GuardianPreflightResourceSignals {
        memory_clamped: signals.contains(&CopyResourceSignal::MemoryClamped),
        low_memory_allocation: signals.contains(&CopyResourceSignal::LowMemoryAllocation),
        memory_pressure: signals.contains(&CopyResourceSignal::MemoryPressure),
        cpu_pressure: signals.contains(&CopyResourceSignal::CpuPressure),
        install_pressure: signals.contains(&CopyResourceSignal::InstallPressure),
        disk_pressure: signals.contains(&CopyResourceSignal::DiskPressure),
    }
}

fn assert_snapshot_coverage(snapshot: &PreflightCopySnapshot) {
    assert_eq!(snapshot.schema, CopySnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), CASE_COUNT);
    let ids = snapshot
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, EXPECTED_CASE_IDS);
    assert_eq!(
        ids.iter().copied().collect::<HashSet<_>>().len(),
        CASE_COUNT
    );
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));

    assert_axis(snapshot, CopyAxis::BoundaryRef, BOUNDARY_CASE_COUNT);
    assert_axis(snapshot, CopyAxis::Static, STATIC_CASE_COUNT);
    assert_axis(
        snapshot,
        CopyAxis::HistoricalComposition,
        HISTORICAL_CASE_COUNT,
    );
    assert_boundary_coverage(snapshot);
    assert_static_coordinate_coverage(snapshot);
    assert_historical_coverage(snapshot);

    let bytes = snapshot_bytes(snapshot);
    let text = std::str::from_utf8(&bytes).expect("preflight copy snapshot is UTF-8");
    let lower = text.to_ascii_lowercase();
    for excluded in [
        "/home/",
        "c:\\\\users",
        "/users/",
        "secret",
        "token",
        "-xmx",
        "--username",
        "java.exe",
        "provider_payload",
        "account_id",
        "operation_id",
    ] {
        assert!(!lower.contains(excluded), "fixture contains {excluded}");
    }
    for case in &snapshot.cases {
        assert!(!case.output.summary.is_empty());
        assert!(case.output.summary.len() <= 180, "{} summary", case.id);
        assert!(case.output.details.len() <= 6, "{} details", case.id);
        assert!(case.output.guidance.len() <= 6, "{} guidance", case.id);
        assert!(
            case.output
                .details
                .iter()
                .chain(&case.output.guidance)
                .all(|line| !line.is_empty() && line.len() <= 240),
            "{} line bounds",
            case.id
        );
    }
    assert!(bytes.ends_with(b"\n"));
    assert!(!bytes.contains(&b'\r'));
}

fn assert_axis(snapshot: &PreflightCopySnapshot, axis: CopyAxis, expected: usize) {
    assert_eq!(
        snapshot
            .cases
            .iter()
            .filter(|case| case.axis == axis && case.axis == axis_for_input(&case.input))
            .count(),
        expected
    );
}

fn axis_for_input(input: &PreflightCopyInput) -> CopyAxis {
    match input {
        PreflightCopyInput::BoundaryRef { .. } => CopyAxis::BoundaryRef,
        PreflightCopyInput::Static { .. } => CopyAxis::Static,
        PreflightCopyInput::HistoricalComposition { .. } => CopyAxis::HistoricalComposition,
    }
}

fn assert_boundary_coverage(snapshot: &PreflightCopySnapshot) {
    let actual = snapshot
        .cases
        .iter()
        .filter_map(|case| match &case.input {
            PreflightCopyInput::BoundaryRef { boundary_ref, .. } => Some(boundary_ref.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = committed_preflight_boundary_case_ids();
    assert_eq!(
        actual,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(
        actual.iter().copied().collect::<HashSet<_>>().len(),
        BOUNDARY_CASE_COUNT
    );

    let boundary_inputs = snapshot.cases.iter().filter_map(|case| match &case.input {
        PreflightCopyInput::BoundaryRef {
            boundary_ref,
            historical_overrides,
        } => Some((boundary_ref.as_str(), historical_overrides.as_slice())),
        _ => None,
    });
    let mut override_count = 0;
    for (boundary_ref, overrides) in boundary_inputs {
        match boundary_ref {
            "warning--recent_startup_failure" => assert!(matches!(
                overrides,
                [BoundaryHistoricalOverride::RecentStartupFailure {
                    failure_class: HistoricalCrashClass::OutOfMemory,
                    occurrences: Some(1),
                    latest_observed_today: Some(true),
                    occurrences_today: None,
                    current_memory_mb: None,
                    suggested_memory_mb: None,
                }]
            )),
            "warning--recent_repair_failed" => assert!(matches!(
                overrides,
                [BoundaryHistoricalOverride::RecentRepairFailed {
                    recovery: HistoricalRecovery::JavaRuntime,
                }]
            )),
            "warning--repair_suppressed_until" => assert!(matches!(
                overrides,
                [BoundaryHistoricalOverride::RepairSuppressedUntil {
                    suppression_until,
                }] if suppression_until == "2026-07-11T11:05:00Z"
            )),
            _ => assert!(
                overrides.is_empty(),
                "unexpected override for {boundary_ref}"
            ),
        }
        override_count += overrides.len();
    }
    assert_eq!(override_count, 3);
}

fn assert_static_coordinate_coverage(snapshot: &PreflightCopySnapshot) {
    let mut coordinates = Vec::new();
    for expected in EXPECTED_STATIC_COORDINATES {
        let case = snapshot
            .cases
            .iter()
            .find(|case| case.id == expected.id)
            .unwrap_or_else(|| panic!("missing static copy case {}", expected.id));
        let PreflightCopyInput::Static {
            mode,
            launchable,
            fact_id,
            source,
            explicit_user_intent,
        } = case.input
        else {
            panic!("{} is not a static copy input", expected.id);
        };
        assert_eq!(mode, expected.mode, "{} mode", expected.id);
        assert_eq!(
            launchable, expected.launchable,
            "{} launchable",
            expected.id
        );
        assert_eq!(fact_id, expected.fact_id, "{} fact", expected.id);
        assert_eq!(source, expected.source, "{} source", expected.id);
        assert_eq!(
            explicit_user_intent, expected.explicit_user_intent,
            "{} explicit intent",
            expected.id
        );
        let expected_decision = match expected.id {
            "static--jvm-unsafe-override-warn" | "static--jvm-unsupported-warn" => {
                GuardianActionKind::Warn
            }
            "static--managed-runtime-corrupt-repair" => GuardianActionKind::Repair,
            "static--managed-runtime-missing-record-only" => GuardianActionKind::RecordOnly,
            _ => GuardianActionKind::Block,
        };
        assert_eq!(
            case.output.kernel_decision, expected_decision,
            "{} kernel decision",
            expected.id
        );
        assert_eq!(
            case.output.effective_decision, expected_decision,
            "{} effective decision",
            expected.id
        );
        assert_eq!(
            case.output.phase,
            OperationPhase::Validating,
            "{} phase",
            expected.id
        );
        let coordinate = (mode, launchable, fact_id, source, explicit_user_intent);
        assert!(!coordinates.contains(&coordinate));
        coordinates.push(coordinate);
    }
    assert_eq!(coordinates.len(), STATIC_CASE_COUNT);
}

fn assert_historical_coverage(snapshot: &PreflightCopySnapshot) {
    let expected = [
        "history--occurrence-windows",
        "history--oom-headroom",
        "history--repair-variants",
        "history--saturated-warning-composition",
        "history--suppression-offset",
    ];
    let actual = snapshot
        .cases
        .iter()
        .filter(|case| case.axis == CopyAxis::HistoricalComposition)
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);

    let historical_cases = snapshot
        .cases
        .iter()
        .filter_map(|case| match &case.input {
            PreflightCopyInput::HistoricalComposition {
                facts, resources, ..
            } => Some((case, facts, resources)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let crash_inputs = historical_cases
        .iter()
        .flat_map(|(_, facts, _)| facts.iter())
        .filter_map(|fact| match fact {
            HistoricalFactInput::StartupFailure {
                failure_class,
                occurrences,
                latest_observed_today,
                occurrences_today,
                current_memory_mb,
                suggested_memory_mb,
                ..
            } => Some((
                *failure_class,
                *occurrences,
                *latest_observed_today,
                *occurrences_today,
                *current_memory_mb,
                *suggested_memory_mb,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    for failure_class in [
        HistoricalCrashClass::OutOfMemory,
        HistoricalCrashClass::GraphicsDriverCrash,
        HistoricalCrashClass::MissingDependency,
        HistoricalCrashClass::ModTransformationFailure,
        HistoricalCrashClass::ModAttributedCrash,
    ] {
        assert!(crash_inputs.iter().any(|input| input.0 == failure_class));
    }
    assert!(
        crash_inputs
            .iter()
            .any(|input| { input.2 == Some(true) && input.3.is_some_and(|count| count > 0) })
    );
    assert!(
        crash_inputs
            .iter()
            .any(|input| { input.1.is_some() && input.2 == Some(true) && input.3.is_none() })
    );
    assert!(
        crash_inputs
            .iter()
            .any(|input| input.1.is_some() && input.2 == Some(false))
    );
    assert!(crash_inputs.iter().any(|input| input.1.is_none()));
    assert!(
        crash_inputs
            .iter()
            .any(|input| input.1 == Some(1) || input.3 == Some(1))
    );
    assert!(crash_inputs.iter().any(|input| {
        input.1.is_some_and(|count| count > 1) || input.3.is_some_and(|count| count > 1)
    }));
    assert!(crash_inputs.iter().any(|input| {
        input.0 == HistoricalCrashClass::OutOfMemory
            && input.4 == Some(4096)
            && input.5 == Some(6144)
    }));
    assert!(crash_inputs.iter().any(|input| {
        input.0 == HistoricalCrashClass::OutOfMemory && input.4 == Some(4096) && input.5.is_none()
    }));

    let recoveries = historical_cases
        .iter()
        .flat_map(|(_, facts, _)| facts.iter())
        .filter_map(|fact| match fact {
            HistoricalFactInput::RepairFailed { recovery, .. } => Some(*recovery),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recoveries,
        [
            HistoricalRecovery::JavaRuntime,
            HistoricalRecovery::JvmArgs,
            HistoricalRecovery::JvmPreset,
        ]
    );

    let (suppression, suppression_facts, suppression_resources) =
        historical_case(snapshot, "history--suppression-offset");
    assert!(suppression_resources.is_empty());
    assert!(suppression_facts.iter().any(|fact| matches!(
        fact,
        HistoricalFactInput::Suppressed {
            suppression_until,
            ..
        } if suppression_until == "2026-07-11T13:45:00+02:00"
    )));
    assert!(
        suppression
            .output
            .details
            .iter()
            .any(|line| line.contains("11:45 UTC"))
    );
    assert!(
        suppression
            .output
            .guidance
            .iter()
            .any(|line| line.contains("11:45 UTC"))
    );

    let (oom, _, _) = historical_case(snapshot, "history--oom-headroom");
    assert!(
        oom.output
            .guidance
            .iter()
            .any(|line| { line.contains("from 4096 MB to 6144 MB") })
    );
    assert!(
        oom.output
            .guidance
            .iter()
            .any(|line| { line.contains("could not verify safe headroom") })
    );

    let (saturated, saturated_facts, saturated_resources) =
        historical_case(snapshot, "history--saturated-warning-composition");
    assert_eq!(saturated_facts.len(), 2);
    assert_eq!(
        saturated_resources,
        [
            CopyResourceSignal::MemoryClamped,
            CopyResourceSignal::LowMemoryAllocation,
            CopyResourceSignal::MemoryPressure,
            CopyResourceSignal::CpuPressure,
            CopyResourceSignal::InstallPressure,
            CopyResourceSignal::DiskPressure,
        ]
    );
    assert_eq!(saturated.output.details.len(), 6);
    assert_eq!(saturated.output.guidance.len(), 6);
    assert!(saturated.output.details[0].contains("out-of-memory crashes today"));
    assert!(saturated.output.details[1].contains("will not auto-repair"));
    assert!(saturated.output.guidance[0].contains("from 4096 MB to 6144 MB"));
    assert!(saturated.output.guidance[1].contains("unchanged settings"));
    assert!(
        saturated
            .output
            .details
            .iter()
            .all(|line| line != "Launch-relevant storage has low free space.")
    );
    assert!(historical_cases.iter().all(|(case, _, _)| {
        case.output.kernel_decision == GuardianActionKind::Warn
            && case.output.effective_decision == GuardianActionKind::Warn
            && case.output.phase == OperationPhase::Validating
    }));
}

fn historical_case<'a>(
    snapshot: &'a PreflightCopySnapshot,
    id: &str,
) -> (
    &'a PreflightCopyCase,
    &'a [HistoricalFactInput],
    &'a [CopyResourceSignal],
) {
    let case = snapshot
        .cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("missing historical copy case {id}"));
    let PreflightCopyInput::HistoricalComposition {
        facts, resources, ..
    } = &case.input
    else {
        panic!("{id} is not a historical composition")
    };
    (case, facts, resources)
}

fn snapshot_bytes(snapshot: &PreflightCopySnapshot) -> Vec<u8> {
    let pretty = serde_json::to_string_pretty(snapshot).expect("serialize preflight copy snapshot");
    format!("{pretty}\n").into_bytes()
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-preflight-copy-v1.json")
}
