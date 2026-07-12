use super::decision_snapshot::{PolicyDecisionCell, scoped_decision_projection};
use super::{
    DiagnosisId, FactReliability, GuardianActionKind, GuardianConfidence, GuardianDirective,
    GuardianDomain, GuardianFact, GuardianFactId, GuardianManagedJavaReason, GuardianMode,
    GuardianPreflightOutcomeRequest, GuardianPreflightOverrideSignals, GuardianPreflightReadiness,
    GuardianPreflightResourceSignals, GuardianSeverity, GuardianStripJvmArgsReason,
    guardian_fact_from_execution, guardian_preflight_outcome,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const SNAPSHOT_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-preflight-boundary-snapshot-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_PREFLIGHT_BOUNDARY_SNAPSHOT";
const FALLBACK_CASE_COUNT: usize = 5;
const STRIP_CASE_COUNT: usize = 8;
const WARNING_CASE_COUNT: usize = 13;
const PRECEDENCE_CASE_COUNT: usize = 9;
const CASE_COUNT: usize =
    FALLBACK_CASE_COUNT + STRIP_CASE_COUNT + WARNING_CASE_COUNT + PRECEDENCE_CASE_COUNT;

const EXPECTED_CASE_IDS: [&str; CASE_COUNT] = [
    "fallback--java_major_mismatch",
    "fallback--java_override_missing",
    "fallback--java_override_undefined_sentinel",
    "fallback--java_probe_failed",
    "fallback--java_update_too_old",
    "strip--jvm_arg_agent_override",
    "strip--jvm_arg_memory_conflict",
    "strip--jvm_arg_reserved_launcher_flag",
    "strip--jvm_arg_unlock_order_invalid",
    "strip--jvm_arg_unsafe_classpath_override",
    "strip--jvm_arg_unsafe_native_path_override",
    "strip--jvm_arg_unsupported_gc",
    "strip--jvm_args_parse_failed",
    "warning--custom_java_override_present",
    "warning--custom_jvm_args_present",
    "warning--custom_jvm_preset_present",
    "warning--java_override_empty",
    "warning--launch_memory_allocation_low",
    "warning--launch_memory_min_clamped",
    "warning--launch_resource_cpu_pressure",
    "warning--launch_resource_disk_pressure",
    "warning--launch_resource_install_pressure",
    "warning--launch_resource_memory_pressure",
    "warning--recent_repair_failed",
    "warning--recent_startup_failure",
    "warning--repair_suppressed_until",
    "precedence--client-jar-missing-block",
    "precedence--disabled-jvm-args-block",
    "precedence--empty-record-only-permitted",
    "precedence--java-override-custom-ask-user",
    "precedence--jvm-args-custom-warning-over-ask-user",
    "precedence--managed-runtime-missing-readiness-block-over-record-only",
    "precedence--readiness-flag-block",
    "precedence--readiness-over-strip",
    "precedence--strip-over-resource-warning",
];

const FALLBACK_FACT_IDS: [GuardianFactId; FALLBACK_CASE_COUNT] = [
    GuardianFactId::JavaOverrideMissing,
    GuardianFactId::JavaOverrideUndefinedSentinel,
    GuardianFactId::JavaProbeFailed,
    GuardianFactId::JavaMajorMismatch,
    GuardianFactId::JavaUpdateTooOld,
];
const STRIP_FACT_IDS: [GuardianFactId; STRIP_CASE_COUNT] = [
    GuardianFactId::JvmArgsParseFailed,
    GuardianFactId::JvmArgReservedLauncherFlag,
    GuardianFactId::JvmArgMemoryConflict,
    GuardianFactId::JvmArgUnsupportedGc,
    GuardianFactId::JvmArgUnlockOrderInvalid,
    GuardianFactId::JvmArgUnsafeClasspathOverride,
    GuardianFactId::JvmArgUnsafeNativePathOverride,
    GuardianFactId::JvmArgAgentOverride,
];
const WARNING_FACT_IDS: [GuardianFactId; WARNING_CASE_COUNT] = [
    GuardianFactId::JavaOverrideEmpty,
    GuardianFactId::LaunchMemoryMinClamped,
    GuardianFactId::LaunchMemoryAllocationLow,
    GuardianFactId::LaunchResourceMemoryPressure,
    GuardianFactId::LaunchResourceCpuPressure,
    GuardianFactId::LaunchResourceInstallPressure,
    GuardianFactId::LaunchResourceDiskPressure,
    GuardianFactId::CustomJavaOverridePresent,
    GuardianFactId::CustomJvmPresetPresent,
    GuardianFactId::CustomJvmArgsPresent,
    GuardianFactId::RecentStartupFailure,
    GuardianFactId::RecentRepairFailed,
    GuardianFactId::RepairSuppressedUntil,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.preflight_boundary_snapshot.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianPreflightBoundarySnapshot {
    schema: SnapshotSchema,
    cases: Vec<BoundaryCase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryCase {
    id: String,
    family: BoundaryFamily,
    input: InputProjection,
    diagnosis_ids: Vec<DiagnosisId>,
    kernel_decision: PolicyDecisionCell,
    effective_decision: GuardianActionKind,
    directives: Vec<PreflightDirectiveProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum PreflightDirectiveProjection {
    UseManagedJavaForAttempt,
    StripExplicitJvmArgsForAttempt,
}

impl From<&GuardianDirective> for PreflightDirectiveProjection {
    fn from(directive: &GuardianDirective) -> Self {
        match directive {
            GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::Preflight,
            } => Self::UseManagedJavaForAttempt,
            GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::Preflight,
            } => Self::StripExplicitJvmArgsForAttempt,
            GuardianDirective::UseManagedJava {
                reason:
                    GuardianManagedJavaReason::PrepareFailure
                    | GuardianManagedJavaReason::StartupRecovery,
            }
            | GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            }
            | GuardianDirective::DowngradeJvmPreset { .. }
            | GuardianDirective::DisableCustomGc => {
                panic!("preflight snapshot received recovery-only directive")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BoundaryFamily {
    Fallback,
    Strip,
    Warning,
    Precedence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputProjection {
    mode: GuardianMode,
    launchable: bool,
    direct_fact_ids: Vec<GuardianFactId>,
    readiness_fact_ids: Vec<GuardianFactId>,
    resource_signals: Vec<ResourceSignal>,
    override_signals: Vec<OverrideSignal>,
    explicit_user_intent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResourceSignal {
    MemoryClamped,
    LowMemoryAllocation,
    MemoryPressure,
    CpuPressure,
    InstallPressure,
    DiskPressure,
}

impl ResourceSignal {
    fn fact_id(self) -> GuardianFactId {
        match self {
            Self::MemoryClamped => GuardianFactId::LaunchMemoryMinClamped,
            Self::LowMemoryAllocation => GuardianFactId::LaunchMemoryAllocationLow,
            Self::MemoryPressure => GuardianFactId::LaunchResourceMemoryPressure,
            Self::CpuPressure => GuardianFactId::LaunchResourceCpuPressure,
            Self::InstallPressure => GuardianFactId::LaunchResourceInstallPressure,
            Self::DiskPressure => GuardianFactId::LaunchResourceDiskPressure,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OverrideSignal {
    Java,
    JvmPreset,
    JvmArgs,
}

impl OverrideSignal {
    fn fact_id(self) -> GuardianFactId {
        match self {
            Self::Java => GuardianFactId::CustomJavaOverridePresent,
            Self::JvmPreset => GuardianFactId::CustomJvmPresetPresent,
            Self::JvmArgs => GuardianFactId::CustomJvmArgsPresent,
        }
    }
}

#[test]
fn checked_in_guardian_preflight_boundary_snapshot_is_byte_stable_and_complete() {
    let fixture = serde_json::from_str::<GuardianPreflightBoundarySnapshot>(SNAPSHOT_FIXTURE)
        .expect("strict Guardian preflight boundary snapshot fixture");
    let generated = build_snapshot();
    let first = snapshot_bytes(&generated);
    let second = snapshot_bytes(&build_snapshot());

    assert_snapshot_coverage(&fixture);
    assert_eq!(fixture, generated);
    assert_eq!(first, second);
    assert_eq!(first.as_slice(), SNAPSHOT_FIXTURE.as_bytes());
}

#[test]
fn guardian_preflight_boundary_snapshot_rejects_nested_fields() {
    let value = serde_json::from_str::<serde_json::Value>(SNAPSHOT_FIXTURE)
        .expect("preflight boundary snapshot JSON");
    let mut input = value.clone();
    input["cases"][0]["input"]["unexpected"] = serde_json::Value::Bool(true);
    assert_unknown_field_rejected(input);

    let mut diagnosis = value.clone();
    diagnosis["cases"][0]["kernel_decision"]["unexpected"] = serde_json::Value::Bool(true);
    assert_unknown_field_rejected(diagnosis);

    let mut case = value;
    case["cases"][0]["unexpected"] = serde_json::Value::Bool(true);
    assert_unknown_field_rejected(case);
}

fn assert_unknown_field_rejected(value: serde_json::Value) {
    let error = serde_json::from_value::<GuardianPreflightBoundarySnapshot>(value)
        .expect_err("nested snapshot fields must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_preflight_boundary_snapshot_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian preflight boundary snapshot"
    );
    std::fs::write(snapshot_fixture_path(), snapshot_bytes(&build_snapshot()))
        .expect("write regenerated Guardian preflight boundary snapshot");
}

fn build_snapshot() -> GuardianPreflightBoundarySnapshot {
    let mut cases = source_family_cases();
    cases.extend(precedence_cases());
    cases.sort_by(|left, right| left.id.cmp(&right.id));
    GuardianPreflightBoundarySnapshot {
        schema: SnapshotSchema::V1,
        cases,
    }
}

pub(super) fn committed_preflight_boundary_case_ids() -> Vec<String> {
    committed_preflight_boundary_snapshot()
        .cases
        .into_iter()
        .map(|case| case.id)
        .collect()
}

pub(super) fn replay_committed_preflight_boundary_case(
    case_id: &str,
    field_overrides: &[(GuardianFactId, Vec<EvidenceField>)],
) -> Option<super::GuardianPreflightOutcome> {
    let snapshot = committed_preflight_boundary_snapshot();
    let case = snapshot.cases.into_iter().find(|case| case.id == case_id)?;
    let mut spec = CaseSpec {
        mode: case.input.mode,
        launchable: case.input.launchable,
        facts: case
            .input
            .direct_fact_ids
            .into_iter()
            .map(producer_fact)
            .collect(),
        readiness_facts: case
            .input
            .readiness_fact_ids
            .into_iter()
            .map(producer_fact)
            .collect(),
        resource_signals: case.input.resource_signals,
        override_signals: case.input.override_signals,
        explicit_user_intent: case.input.explicit_user_intent,
    };
    for fact in spec.facts.iter_mut().chain(&mut spec.readiness_facts) {
        fact.fields.clear();
        if let Some((_, fields)) = field_overrides.iter().find(|(id, _)| *id == fact.id) {
            fact.fields = fields.clone();
        }
    }
    assert!(field_overrides.iter().all(|(id, _)| {
        spec.facts
            .iter()
            .chain(&spec.readiness_facts)
            .any(|fact| fact.id == *id)
    }));
    let resources = resource_signals(&spec.resource_signals);
    let overrides = override_signals(&spec.override_signals);
    Some(guardian_preflight_outcome(
        GuardianPreflightOutcomeRequest {
            operation_id: None,
            mode: spec.mode,
            phase: OperationPhase::Validating,
            facts: &spec.facts,
            readiness: GuardianPreflightReadiness::from_facts(
                spec.launchable,
                &spec.readiness_facts,
            ),
            resources,
            overrides,
            explicit_user_intent: spec.explicit_user_intent,
        },
    ))
}

fn committed_preflight_boundary_snapshot() -> GuardianPreflightBoundarySnapshot {
    serde_json::from_str(SNAPSHOT_FIXTURE)
        .expect("strict committed Guardian preflight boundary snapshot fixture")
}

fn source_family_cases() -> Vec<BoundaryCase> {
    let mut cases = FALLBACK_FACT_IDS
        .into_iter()
        .map(|id| {
            boundary_case(
                format!("fallback--{}", id.as_str()),
                BoundaryFamily::Fallback,
                CaseSpec::with_fact(GuardianMode::Managed, producer_fact(id)),
            )
        })
        .collect::<Vec<_>>();
    cases.extend(STRIP_FACT_IDS.into_iter().map(|id| {
        boundary_case(
            format!("strip--{}", id.as_str()),
            BoundaryFamily::Strip,
            CaseSpec::with_fact(GuardianMode::Managed, producer_fact(id)),
        )
    }));
    cases.push(boundary_case(
        "warning--java_override_empty",
        BoundaryFamily::Warning,
        CaseSpec::with_fact(
            GuardianMode::Managed,
            producer_fact(GuardianFactId::JavaOverrideEmpty),
        ),
    ));
    for signal in [
        ResourceSignal::MemoryClamped,
        ResourceSignal::LowMemoryAllocation,
        ResourceSignal::MemoryPressure,
        ResourceSignal::CpuPressure,
        ResourceSignal::InstallPressure,
        ResourceSignal::DiskPressure,
    ] {
        cases.push(boundary_case(
            format!("warning--{}", signal.fact_id().as_str()),
            BoundaryFamily::Warning,
            CaseSpec::with_resource(signal),
        ));
    }
    for signal in [
        OverrideSignal::Java,
        OverrideSignal::JvmPreset,
        OverrideSignal::JvmArgs,
    ] {
        cases.push(boundary_case(
            format!("warning--{}", signal.fact_id().as_str()),
            BoundaryFamily::Warning,
            CaseSpec::with_override(signal),
        ));
    }
    for id in [
        GuardianFactId::RecentStartupFailure,
        GuardianFactId::RecentRepairFailed,
        GuardianFactId::RepairSuppressedUntil,
    ] {
        cases.push(boundary_case(
            format!("warning--{}", id.as_str()),
            BoundaryFamily::Warning,
            CaseSpec::with_fact(GuardianMode::Managed, producer_fact(id)),
        ));
    }
    cases
}

fn precedence_cases() -> Vec<BoundaryCase> {
    vec![
        boundary_case(
            "precedence--empty-record-only-permitted",
            BoundaryFamily::Precedence,
            CaseSpec::empty(GuardianMode::Managed),
        ),
        boundary_case(
            "precedence--readiness-flag-block",
            BoundaryFamily::Precedence,
            CaseSpec::empty(GuardianMode::Managed).with_launchable(false),
        ),
        boundary_case(
            "precedence--managed-runtime-missing-readiness-block-over-record-only",
            BoundaryFamily::Precedence,
            CaseSpec::with_fact(
                GuardianMode::Managed,
                producer_fact(GuardianFactId::ManagedRuntimeMissing),
            )
            .with_launchable(false),
        ),
        boundary_case(
            "precedence--client-jar-missing-block",
            BoundaryFamily::Precedence,
            CaseSpec::empty(GuardianMode::Managed)
                .with_readiness_fact(producer_fact(GuardianFactId::ClientJarMissing)),
        ),
        boundary_case(
            "precedence--java-override-custom-ask-user",
            BoundaryFamily::Precedence,
            CaseSpec::with_fact(
                GuardianMode::Custom,
                producer_fact(GuardianFactId::JavaOverrideMissing),
            )
            .with_explicit_user_intent(),
        ),
        boundary_case(
            "precedence--jvm-args-custom-warning-over-ask-user",
            BoundaryFamily::Precedence,
            CaseSpec::with_fact(
                GuardianMode::Custom,
                producer_fact(GuardianFactId::JvmArgsParseFailed),
            )
            .with_explicit_user_intent(),
        ),
        boundary_case(
            "precedence--disabled-jvm-args-block",
            BoundaryFamily::Precedence,
            CaseSpec::with_fact(
                GuardianMode::Disabled,
                producer_fact(GuardianFactId::JvmArgsParseFailed),
            )
            .with_explicit_user_intent(),
        ),
        boundary_case(
            "precedence--readiness-over-strip",
            BoundaryFamily::Precedence,
            CaseSpec::with_fact(
                GuardianMode::Managed,
                producer_fact(GuardianFactId::JvmArgsParseFailed),
            )
            .with_launchable(false)
            .with_explicit_user_intent(),
        ),
        boundary_case(
            "precedence--strip-over-resource-warning",
            BoundaryFamily::Precedence,
            CaseSpec::with_fact(
                GuardianMode::Managed,
                producer_fact(GuardianFactId::JvmArgsParseFailed),
            )
            .with_resource_signal(ResourceSignal::MemoryPressure)
            .with_explicit_user_intent(),
        ),
    ]
}

#[derive(Clone, Debug)]
struct CaseSpec {
    mode: GuardianMode,
    launchable: bool,
    facts: Vec<GuardianFact>,
    readiness_facts: Vec<GuardianFact>,
    resource_signals: Vec<ResourceSignal>,
    override_signals: Vec<OverrideSignal>,
    explicit_user_intent: bool,
}

impl CaseSpec {
    fn empty(mode: GuardianMode) -> Self {
        Self {
            mode,
            launchable: true,
            facts: Vec::new(),
            readiness_facts: Vec::new(),
            resource_signals: Vec::new(),
            override_signals: Vec::new(),
            explicit_user_intent: false,
        }
    }

    fn with_fact(mode: GuardianMode, fact: GuardianFact) -> Self {
        let mut spec = Self::empty(mode);
        spec.facts.push(fact);
        spec
    }

    fn with_resource(signal: ResourceSignal) -> Self {
        Self::empty(GuardianMode::Managed).with_resource_signal(signal)
    }

    fn with_override(signal: OverrideSignal) -> Self {
        let mut spec = Self::empty(GuardianMode::Custom);
        spec.override_signals.push(signal);
        spec
    }

    fn with_launchable(mut self, launchable: bool) -> Self {
        self.launchable = launchable;
        self
    }

    fn with_readiness_fact(mut self, fact: GuardianFact) -> Self {
        self.launchable = false;
        self.readiness_facts.push(fact);
        self
    }

    fn with_resource_signal(mut self, signal: ResourceSignal) -> Self {
        self.resource_signals.push(signal);
        self
    }

    fn with_explicit_user_intent(mut self) -> Self {
        self.explicit_user_intent = true;
        self
    }
}

fn boundary_case(id: impl Into<String>, family: BoundaryFamily, spec: CaseSpec) -> BoundaryCase {
    let resources = resource_signals(&spec.resource_signals);
    let overrides = override_signals(&spec.override_signals);
    let input = InputProjection {
        mode: spec.mode,
        launchable: spec.launchable,
        direct_fact_ids: spec.facts.iter().map(|fact| fact.id).collect(),
        readiness_fact_ids: spec.readiness_facts.iter().map(|fact| fact.id).collect(),
        resource_signals: spec.resource_signals.clone(),
        override_signals: spec.override_signals.clone(),
        explicit_user_intent: spec.explicit_user_intent,
    };
    let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
        operation_id: None,
        mode: spec.mode,
        phase: OperationPhase::Validating,
        facts: &spec.facts,
        readiness: GuardianPreflightReadiness::from_facts(spec.launchable, &spec.readiness_facts),
        resources,
        overrides,
        explicit_user_intent: spec.explicit_user_intent,
    });
    assert_eq!(outcome.safety.decision, outcome.user_outcome.decision());
    let plan = outcome
        .guardian_decision
        .action_plan
        .as_ref()
        .expect("preflight decision has an action plan");
    assert!(
        plan.prerequisite
            .candidate_actions
            .contains(&outcome.guardian_decision.kind),
        "scoped verdict is absent from prerequisite candidates"
    );
    BoundaryCase {
        id: id.into(),
        family,
        input,
        diagnosis_ids: outcome
            .safety_case
            .diagnoses
            .iter()
            .map(|diagnosis| diagnosis.id())
            .collect(),
        kernel_decision: scoped_decision_projection(
            &outcome.guardian_decision,
            &outcome.safety_case,
        ),
        effective_decision: outcome.user_outcome.decision(),
        directives: outcome
            .directives
            .iter()
            .map(PreflightDirectiveProjection::from)
            .collect(),
    }
}

fn resource_signals(signals: &[ResourceSignal]) -> GuardianPreflightResourceSignals {
    GuardianPreflightResourceSignals {
        memory_clamped: signals.contains(&ResourceSignal::MemoryClamped),
        low_memory_allocation: signals.contains(&ResourceSignal::LowMemoryAllocation),
        memory_pressure: signals.contains(&ResourceSignal::MemoryPressure),
        cpu_pressure: signals.contains(&ResourceSignal::CpuPressure),
        install_pressure: signals.contains(&ResourceSignal::InstallPressure),
        disk_pressure: signals.contains(&ResourceSignal::DiskPressure),
    }
}

fn override_signals(signals: &[OverrideSignal]) -> GuardianPreflightOverrideSignals {
    GuardianPreflightOverrideSignals {
        explicit_java_override: signals.contains(&OverrideSignal::Java),
        explicit_jvm_preset: signals.contains(&OverrideSignal::JvmPreset),
        explicit_jvm_args: signals.contains(&OverrideSignal::JvmArgs),
    }
}

fn producer_fact(id: GuardianFactId) -> GuardianFact {
    if let Some(kind) = execution_fact_kind(id) {
        let ownership = OwnershipClass::UserOwned;
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind,
                target: Some(TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    TargetKind::Config,
                    "guardian_preflight_snapshot",
                    ownership,
                )),
                fields: Vec::new(),
            },
            OperationPhase::Validating,
        );
        assert_eq!(fact.id, id);
        return fact;
    }

    let (domain, reliability, severity, ownership, kind, system, target_id) = match id {
        GuardianFactId::ManagedRuntimeMissing => (
            GuardianDomain::Runtime,
            FactReliability::ExpectedMarkerAbsence,
            GuardianSeverity::Recoverable,
            OwnershipClass::LauncherManaged,
            TargetKind::Runtime,
            StabilizationSystem::Execution,
            "managed_runtime",
        ),
        GuardianFactId::ClientJarMissing => (
            GuardianDomain::Install,
            FactReliability::ExpectedMarkerAbsence,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Artifact,
            StabilizationSystem::Execution,
            "client_jar",
        ),
        GuardianFactId::RecentStartupFailure => (
            GuardianDomain::Startup,
            FactReliability::DirectStructured,
            GuardianSeverity::Warning,
            OwnershipClass::UserOwned,
            TargetKind::Instance,
            StabilizationSystem::Guardian,
            "instance",
        ),
        GuardianFactId::RecentRepairFailed | GuardianFactId::RepairSuppressedUntil => (
            GuardianDomain::Launch,
            FactReliability::DirectStructured,
            GuardianSeverity::Warning,
            OwnershipClass::LauncherManaged,
            TargetKind::Instance,
            StabilizationSystem::Guardian,
            "instance",
        ),
        _ => panic!("missing preflight snapshot producer for {}", id.as_str()),
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
        target: Some(TargetDescriptor::new(system, kind, target_id, ownership)),
        fields: historical_fields(id),
    }
}

fn historical_fields(id: GuardianFactId) -> Vec<EvidenceField> {
    let values = match id {
        GuardianFactId::RecentStartupFailure => vec![
            ("failure_class", "out_of_memory"),
            ("occurrences", "1"),
            ("latest_observed_today", "true"),
        ],
        GuardianFactId::RecentRepairFailed => vec![("diagnosis", "java_runtime_recovery")],
        GuardianFactId::RepairSuppressedUntil => vec![
            ("diagnosis", "java_runtime_recovery"),
            ("suppression_until", "2026-07-11T11:05:00Z"),
        ],
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(|(key, value)| EvidenceField::new(key, value, EvidenceSensitivity::Public))
        .collect()
}

fn execution_fact_kind(id: GuardianFactId) -> Option<ExecutionFactKind> {
    Some(match id {
        GuardianFactId::JavaOverrideEmpty => ExecutionFactKind::RuntimeJavaOverrideEmpty,
        GuardianFactId::JavaOverrideMissing => ExecutionFactKind::RuntimeMissingExecutable,
        GuardianFactId::JavaOverrideUndefinedSentinel => {
            ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel
        }
        GuardianFactId::JavaProbeFailed => ExecutionFactKind::RuntimeProbeFailed,
        GuardianFactId::JavaMajorMismatch => ExecutionFactKind::RuntimeWrongMajor,
        GuardianFactId::JavaUpdateTooOld => ExecutionFactKind::RuntimeWrongUpdate,
        GuardianFactId::JvmArgsParseFailed => ExecutionFactKind::JvmArgsParseFailed,
        GuardianFactId::JvmArgReservedLauncherFlag => ExecutionFactKind::JvmArgReservedLauncherFlag,
        GuardianFactId::JvmArgMemoryConflict => ExecutionFactKind::JvmArgMemoryConflict,
        GuardianFactId::JvmArgUnsupportedGc => ExecutionFactKind::JvmArgUnsupportedGc,
        GuardianFactId::JvmArgUnlockOrderInvalid => ExecutionFactKind::JvmArgUnlockOrderInvalid,
        GuardianFactId::JvmArgUnsafeClasspathOverride => {
            ExecutionFactKind::JvmArgUnsafeClasspathOverride
        }
        GuardianFactId::JvmArgUnsafeNativePathOverride => {
            ExecutionFactKind::JvmArgUnsafeNativePathOverride
        }
        GuardianFactId::JvmArgAgentOverride => ExecutionFactKind::JvmArgAgentOverride,
        _ => return None,
    })
}

fn assert_snapshot_coverage(snapshot: &GuardianPreflightBoundarySnapshot) {
    assert_eq!(snapshot.schema, SnapshotSchema::V1);
    assert_eq!(snapshot.cases.len(), CASE_COUNT);
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

    assert_family(snapshot, BoundaryFamily::Fallback, FALLBACK_CASE_COUNT);
    assert_family(snapshot, BoundaryFamily::Strip, STRIP_CASE_COUNT);
    assert_family(snapshot, BoundaryFamily::Warning, WARNING_CASE_COUNT);
    assert_family(snapshot, BoundaryFamily::Precedence, PRECEDENCE_CASE_COUNT);
    assert_eq!(
        family_fact_inventory(snapshot, BoundaryFamily::Fallback),
        sorted_fact_ids(FALLBACK_FACT_IDS)
    );
    assert_eq!(
        family_fact_inventory(snapshot, BoundaryFamily::Strip),
        sorted_fact_ids(STRIP_FACT_IDS)
    );
    assert_eq!(
        family_fact_inventory(snapshot, BoundaryFamily::Warning),
        sorted_fact_ids(WARNING_FACT_IDS)
    );

    assert!(
        snapshot
            .cases
            .iter()
            .all(|case| case.kernel_decision.plan_integrity)
    );
    for case in &snapshot.cases {
        if case.id == "precedence--java-override-custom-ask-user" {
            assert_eq!(
                case.kernel_decision.decision_kind,
                GuardianActionKind::AskUser
            );
            assert_eq!(case.effective_decision, GuardianActionKind::Block);
        } else {
            assert_eq!(
                case.kernel_decision.decision_kind, case.effective_decision,
                "unexpected preflight boundary adaptation in {}",
                case.id
            );
        }
    }
    assert!(
        snapshot
            .cases
            .iter()
            .all(|case| !case.diagnosis_ids.is_empty())
    );
    assert_effective_decision(
        snapshot,
        "precedence--empty-record-only-permitted",
        GuardianActionKind::RecordOnly,
    );
    assert_effective_decision(
        snapshot,
        "precedence--readiness-flag-block",
        GuardianActionKind::Block,
    );
    assert_effective_decision(
        snapshot,
        "precedence--managed-runtime-missing-readiness-block-over-record-only",
        GuardianActionKind::Block,
    );
    assert_effective_decision(
        snapshot,
        "precedence--client-jar-missing-block",
        GuardianActionKind::Block,
    );
    assert_effective_decision(
        snapshot,
        "precedence--java-override-custom-ask-user",
        GuardianActionKind::Block,
    );
    assert_effective_decision(
        snapshot,
        "precedence--jvm-args-custom-warning-over-ask-user",
        GuardianActionKind::Warn,
    );
    assert_effective_decision(
        snapshot,
        "precedence--disabled-jvm-args-block",
        GuardianActionKind::Block,
    );
    assert_effective_decision(
        snapshot,
        "precedence--readiness-over-strip",
        GuardianActionKind::Block,
    );
    assert_effective_decision(
        snapshot,
        "precedence--strip-over-resource-warning",
        GuardianActionKind::Strip,
    );

    let fallback = snapshot
        .cases
        .iter()
        .filter(|case| case.family == BoundaryFamily::Fallback)
        .collect::<Vec<_>>();
    assert!(fallback.iter().all(|case| {
        case.effective_decision == GuardianActionKind::Fallback
            && case.directives == [PreflightDirectiveProjection::UseManagedJavaForAttempt]
    }));
    let strip = snapshot
        .cases
        .iter()
        .filter(|case| case.family == BoundaryFamily::Strip)
        .collect::<Vec<_>>();
    assert!(strip.iter().all(|case| {
        case.effective_decision == GuardianActionKind::Strip
            && case.directives == [PreflightDirectiveProjection::StripExplicitJvmArgsForAttempt]
    }));
    assert!(snapshot.cases.iter().all(|case| {
        case.family != BoundaryFamily::Warning
            || (case.effective_decision == GuardianActionKind::Warn && case.directives.is_empty())
    }));

    let bytes = snapshot_bytes(snapshot);
    let text = std::str::from_utf8(&bytes).expect("snapshot is UTF-8");
    for excluded in [
        "summary",
        "details",
        "guidance",
        "public_reason",
        "user_outcome",
        "operation_id",
        "target",
        "fields",
        "/home/",
        "C:\\\\Users",
        "--accessToken",
        "-Xmx",
    ] {
        assert!(!text.contains(excluded), "fixture contains {excluded}");
    }
    assert!(bytes.ends_with(b"\n"));
    assert!(!bytes.contains(&b'\r'));
}

fn assert_family(
    snapshot: &GuardianPreflightBoundarySnapshot,
    family: BoundaryFamily,
    count: usize,
) {
    assert_eq!(
        snapshot
            .cases
            .iter()
            .filter(|case| case.family == family)
            .count(),
        count
    );
}

fn family_fact_inventory(
    snapshot: &GuardianPreflightBoundarySnapshot,
    family: BoundaryFamily,
) -> Vec<GuardianFactId> {
    let mut ids = snapshot
        .cases
        .iter()
        .filter(|case| case.family == family)
        .flat_map(|case| {
            case.input
                .direct_fact_ids
                .iter()
                .copied()
                .chain(
                    case.input
                        .resource_signals
                        .iter()
                        .map(|signal| signal.fact_id()),
                )
                .chain(
                    case.input
                        .override_signals
                        .iter()
                        .map(|signal| signal.fact_id()),
                )
        })
        .collect::<Vec<_>>();
    ids.sort_unstable_by_key(GuardianFactId::as_str);
    ids
}

fn sorted_fact_ids<const N: usize>(ids: [GuardianFactId; N]) -> Vec<GuardianFactId> {
    let mut ids = ids.to_vec();
    ids.sort_unstable_by_key(GuardianFactId::as_str);
    ids
}

fn assert_effective_decision(
    snapshot: &GuardianPreflightBoundarySnapshot,
    id: &str,
    expected: GuardianActionKind,
) {
    let case = snapshot
        .cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("missing {id}"));
    assert_eq!(case.effective_decision, expected, "{id}");
}

fn snapshot_bytes(snapshot: &GuardianPreflightBoundarySnapshot) -> Vec<u8> {
    let pretty =
        serde_json::to_string_pretty(snapshot).expect("serialize preflight boundary snapshot");
    format!("{pretty}\n").into_bytes()
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-preflight-boundary-snapshot-v1.json")
}
