use super::inference_graph::{
    DiagnosisGraphNode, DomainTemplate, FactRequirement, diagnosis_graph_nodes,
};
use super::{
    Diagnosis, DiagnosisId, FactReliability, GuardianActionKind, GuardianConfidence,
    GuardianDomain, GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext,
    GuardianSeverity, SafetyCase, decide_guardian_policy, diagnose_facts,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

const SNAPSHOT_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-decision-snapshot-v1.json"
));
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_DECISION_SNAPSHOT";
const GRAPH_NODE_COUNT: usize = 34;
const GRAPH_SOURCE_COUNT: usize = 69;
const GRAPH_DIAGNOSIS_COUNT: usize = 46;
const GRAPH_SOURCE_PHASE_COUNT: usize = 272;
const UNKNOWN_SOURCE_COUNT: usize = 12;
const CONTEXT_COUNT: usize = 16;
const GRAPH_OWNERSHIP_COUNT: usize = 5;
const MODE_COUNT: usize = 3;
const POLICY_CELLS_PER_ROOT: usize = CONTEXT_COUNT * MODE_COUNT;
const POLICY_ROOT_COUNT: usize = GRAPH_SOURCE_COUNT * GRAPH_OWNERSHIP_COUNT + UNKNOWN_SOURCE_COUNT;
const RAW_DIAGNOSIS_CASE_COUNT: usize =
    GRAPH_SOURCE_PHASE_COUNT * GRAPH_OWNERSHIP_COUNT + UNKNOWN_SOURCE_COUNT;
const RAW_POLICY_EVALUATION_COUNT: usize = RAW_DIAGNOSIS_CASE_COUNT * POLICY_CELLS_PER_ROOT;
const COMPRESSED_POLICY_CELL_COUNT: usize = POLICY_ROOT_COUNT * POLICY_CELLS_PER_ROOT;

const MODES: [GuardianMode; MODE_COUNT] = [
    GuardianMode::Managed,
    GuardianMode::Custom,
    GuardianMode::Disabled,
];
const OWNERSHIPS: [OwnershipClass; GRAPH_OWNERSHIP_COUNT] = [
    OwnershipClass::LauncherManaged,
    OwnershipClass::CompositionManaged,
    OwnershipClass::UserOwned,
    OwnershipClass::ExternalProviderDerived,
    OwnershipClass::Unknown,
];
const PHASES: [OperationPhase; UNKNOWN_SOURCE_COUNT] = [
    OperationPhase::Startup,
    OperationPhase::Planning,
    OperationPhase::Validating,
    OperationPhase::Downloading,
    OperationPhase::Installing,
    OperationPhase::Preparing,
    OperationPhase::Launching,
    OperationPhase::Running,
    OperationPhase::Repairing,
    OperationPhase::RollingBack,
    OperationPhase::Completed,
    OperationPhase::Failed,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SnapshotSchema {
    #[serde(rename = "axial.guardian.decision_snapshot.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardianDecisionSnapshot {
    schema: SnapshotSchema,
    contexts: Vec<PolicyContextCoordinate>,
    source_cases: Vec<SourceCase>,
    policy_profiles: Vec<PolicyProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyContextCoordinate {
    id: String,
    journal_available: bool,
    suppression_active: bool,
    public_redaction_ready: bool,
    explicit_user_intent: bool,
}

impl PolicyContextCoordinate {
    fn policy_context(&self) -> GuardianPolicyContext {
        GuardianPolicyContext {
            journal_available: self.journal_available,
            suppression_active: self.suppression_active,
            public_redaction_ready: self.public_redaction_ready,
            explicit_user_intent: self.explicit_user_intent,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCase {
    id: String,
    input: SourceInput,
    allowed_phases: Vec<OperationPhase>,
    diagnosis: DiagnosisProjection,
    ownership_profiles: Vec<OwnershipProfileRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SourceInput {
    Fact {
        fact_id: GuardianFactId,
        domain: GuardianDomain,
        reliability: FactReliability,
        severity: Option<GuardianSeverity>,
        confidence: Option<GuardianConfidence>,
    },
    Empty,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosisProjection {
    id: DiagnosisId,
    domain: GuardianDomain,
    severity: GuardianSeverity,
    confidence: GuardianConfidence,
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
            fact_ids: diagnosis.fact_ids.clone(),
            candidate_actions: diagnosis.candidate_actions.clone(),
            public_reason_template: diagnosis.public_reason_template.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipProfileRef {
    ownership: OwnershipClass,
    policy_profile: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyProfile {
    id: String,
    modes: Vec<ModePolicyRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModePolicyRow {
    mode: GuardianMode,
    contexts: Vec<PolicyDecisionCell>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PolicyDecisionCell {
    pub(super) decision_kind: GuardianActionKind,
    pub(super) plan_present: bool,
    pub(super) plan_integrity: bool,
}

#[test]
fn checked_in_guardian_decision_snapshot_is_byte_stable_and_complete() {
    let fixture = serde_json::from_str::<GuardianDecisionSnapshot>(SNAPSHOT_FIXTURE)
        .expect("strict Guardian decision snapshot fixture");
    let generated = build_snapshot();

    assert_snapshot_coverage(&fixture);
    assert_eq!(fixture, generated);
    let pretty = serde_json::to_string_pretty(&generated).expect("serialize decision snapshot");
    assert_eq!(format!("{pretty}\n"), SNAPSHOT_FIXTURE);
}

#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_guardian_decision_snapshot_fixture() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to regenerate the Guardian decision snapshot"
    );
    let fixture = serde_json::to_string_pretty(&build_snapshot())
        .expect("serialize regenerated Guardian decision snapshot");
    std::fs::write(snapshot_fixture_path(), format!("{fixture}\n"))
        .expect("write regenerated Guardian decision snapshot");
}

fn build_snapshot() -> GuardianDecisionSnapshot {
    let contexts = policy_contexts();
    let mut matrices = BTreeMap::<String, Vec<ModePolicyRow>>::new();
    let mut source_cases = Vec::new();

    assert_eq!(diagnosis_graph_nodes().len(), GRAPH_NODE_COUNT);
    for node in diagnosis_graph_nodes() {
        for fact_id in graph_source_fact_ids(node) {
            source_cases.push(graph_source_case(node, fact_id, &contexts, &mut matrices));
        }
    }
    assert_eq!(source_cases.len(), GRAPH_SOURCE_COUNT);

    for phase in PHASES {
        source_cases.push(unknown_source_case(phase, &contexts, &mut matrices));
    }
    source_cases.sort_by(|left, right| left.id.cmp(&right.id));

    let policy_profiles = matrices
        .into_iter()
        .map(|(id, modes)| PolicyProfile { id, modes })
        .collect();

    GuardianDecisionSnapshot {
        schema: SnapshotSchema::V1,
        contexts,
        source_cases,
        policy_profiles,
    }
}

fn graph_source_case(
    node: &DiagnosisGraphNode,
    fact_id: GuardianFactId,
    contexts: &[PolicyContextCoordinate],
    matrices: &mut BTreeMap<String, Vec<ModePolicyRow>>,
) -> SourceCase {
    assert!(!node.phase_allowed.is_empty(), "{}", fact_id.as_str());
    let input = graph_source_input(node, fact_id);
    let mut expected_projection = None;
    let mut ownership_profiles = Vec::with_capacity(OWNERSHIPS.len());

    for ownership in OWNERSHIPS {
        let mut expected_matrix = None;
        for phase in node.phase_allowed {
            let diagnosis = graph_source_diagnosis(&input, *phase, ownership);
            let projection = DiagnosisProjection::from(&diagnosis);
            if let Some(expected) = &expected_projection {
                assert_eq!(expected, &projection, "{} at {phase:?}", fact_id.as_str());
            } else {
                expected_projection = Some(projection);
            }

            let matrix = policy_matrix(&diagnosis, contexts);
            if let Some(expected) = &expected_matrix {
                assert_eq!(expected, &matrix, "{} at {phase:?}", fact_id.as_str());
            } else {
                expected_matrix = Some(matrix);
            }
        }

        let matrix = expected_matrix.expect("graph source has an allowed phase");
        let policy_profile = insert_policy_profile(matrices, matrix);
        ownership_profiles.push(OwnershipProfileRef {
            ownership,
            policy_profile,
        });
    }

    let diagnosis = expected_projection.expect("graph source has a diagnosis");
    SourceCase {
        id: format!("{}--{}", diagnosis.id.as_str(), fact_id.as_str()),
        input,
        allowed_phases: node.phase_allowed.to_vec(),
        diagnosis,
        ownership_profiles,
    }
}

fn unknown_source_case(
    phase: OperationPhase,
    contexts: &[PolicyContextCoordinate],
    matrices: &mut BTreeMap<String, Vec<ModePolicyRow>>,
) -> SourceCase {
    let mut diagnoses = diagnose_facts(&[], phase);
    assert_eq!(diagnoses.len(), 1);
    let diagnosis = diagnoses.pop().expect("empty facts produce one diagnosis");
    assert_eq!(diagnosis.ownership, OwnershipClass::Unknown);
    assert_eq!(diagnosis.id, DiagnosisId::UnknownFailure(phase));

    let policy_profile = insert_policy_profile(matrices, policy_matrix(&diagnosis, contexts));
    SourceCase {
        id: format!("{}--empty", diagnosis.id.as_str()),
        input: SourceInput::Empty,
        allowed_phases: vec![phase],
        diagnosis: DiagnosisProjection::from(&diagnosis),
        ownership_profiles: vec![OwnershipProfileRef {
            ownership: OwnershipClass::Unknown,
            policy_profile,
        }],
    }
}

fn graph_source_diagnosis(
    input: &SourceInput,
    phase: OperationPhase,
    ownership: OwnershipClass,
) -> Diagnosis {
    let SourceInput::Fact {
        fact_id,
        domain,
        reliability,
        severity,
        confidence,
        ..
    } = input
    else {
        panic!("graph source must be a fact")
    };
    let fact = GuardianFact {
        operation_id: None,
        id: *fact_id,
        domain: *domain,
        phase,
        reliability: *reliability,
        severity: *severity,
        confidence: *confidence,
        ownership,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            target_kind_for_domain(*domain),
            "guardian_decision_snapshot",
            ownership,
        )),
        fields: Vec::new(),
    };
    let mut diagnoses = diagnose_facts(&[fact], phase);
    assert_eq!(diagnoses.len(), 1, "{} at {phase:?}", fact_id.as_str());
    diagnoses.pop().expect("graph fact produces one diagnosis")
}

fn policy_matrix(
    diagnosis: &Diagnosis,
    contexts: &[PolicyContextCoordinate],
) -> Vec<ModePolicyRow> {
    MODES
        .into_iter()
        .map(|mode| {
            let safety_case = SafetyCase {
                operation_id: None,
                mode,
                phase: diagnosis.phase,
                diagnoses: vec![diagnosis.clone()],
            };
            let contexts = contexts
                .iter()
                .map(|context| {
                    let decision = decide_guardian_policy(&safety_case, context.policy_context());
                    decision_projection(&decision, &safety_case)
                })
                .collect();
            ModePolicyRow { mode, contexts }
        })
        .collect()
}

pub(super) fn decision_projection(
    decision: &super::GuardianDecision,
    safety_case: &SafetyCase,
) -> PolicyDecisionCell {
    PolicyDecisionCell {
        decision_kind: decision.kind,
        plan_present: decision.action_plan.is_some(),
        plan_integrity: decision_plan_integrity(decision, safety_case),
    }
}

fn decision_plan_integrity(decision: &super::GuardianDecision, safety_case: &SafetyCase) -> bool {
    if decision.operation_id != safety_case.operation_id
        || decision.mode != safety_case.mode
        || decision.diagnoses
            != safety_case
                .diagnoses
                .iter()
                .map(|diagnosis| diagnosis.id)
                .collect::<Vec<_>>()
    {
        return false;
    }
    let Some(plan) = &decision.action_plan else {
        return true;
    };
    let [action] = plan.actions.as_slice() else {
        return false;
    };
    let prerequisite_matches = safety_case.diagnoses.iter().any(|diagnosis| {
        plan.prerequisite.diagnosis_id == diagnosis.id
            && plan.prerequisite.ownership == diagnosis.ownership
            && plan.prerequisite.confidence == diagnosis.confidence
            && plan.prerequisite.affected_targets == diagnosis.affected_targets
            && plan.prerequisite.candidate_actions == diagnosis.candidate_actions
    });

    plan.owner == StabilizationSystem::Guardian
        && prerequisite_matches
        && action.kind == decision.kind
        && action.reason == plan.prerequisite.diagnosis_id
        && action.target.as_ref() == plan.prerequisite.affected_targets.first()
}

fn insert_policy_profile(
    profiles: &mut BTreeMap<String, Vec<ModePolicyRow>>,
    modes: Vec<ModePolicyRow>,
) -> String {
    let id = policy_profile_id(&modes);
    if let Some(existing) = profiles.get(&id) {
        assert_eq!(existing, &modes, "policy profile digest collision");
    } else {
        profiles.insert(id.clone(), modes);
    }
    id
}

fn policy_profile_id(modes: &[ModePolicyRow]) -> String {
    let bytes = serde_json::to_vec(modes).expect("serialize policy profile content");
    let digest = Sha256::digest(bytes);
    format!("profile-{}", hex::encode(&digest[..8]))
}

fn policy_contexts() -> Vec<PolicyContextCoordinate> {
    (0_u8..CONTEXT_COUNT as u8)
        .map(|bits| {
            let journal_available = bits & 0b0001 != 0;
            let suppression_active = bits & 0b0010 != 0;
            let public_redaction_ready = bits & 0b0100 != 0;
            let explicit_user_intent = bits & 0b1000 != 0;
            PolicyContextCoordinate {
                id: format!(
                    "j{}-s{}-r{}-u{}",
                    u8::from(journal_available),
                    u8::from(suppression_active),
                    u8::from(public_redaction_ready),
                    u8::from(explicit_user_intent)
                ),
                journal_available,
                suppression_active,
                public_redaction_ready,
                explicit_user_intent,
            }
        })
        .collect()
}

fn graph_source_fact_ids(node: &DiagnosisGraphNode) -> Vec<GuardianFactId> {
    node.required_facts
        .iter()
        .flat_map(|requirement| requirement.source_fact_ids().iter().copied())
        .collect()
}

fn graph_source_input(node: &DiagnosisGraphNode, fact_id: GuardianFactId) -> SourceInput {
    let (severity, confidence) = producer_fact_overrides(fact_id);
    SourceInput::Fact {
        fact_id,
        domain: producer_fact_domain(node, fact_id),
        reliability: producer_fact_reliability(fact_id),
        severity,
        confidence,
    }
}

fn producer_fact_domain(node: &DiagnosisGraphNode, fact_id: GuardianFactId) -> GuardianDomain {
    match fact_id {
        GuardianFactId::LaunchMemoryMinClamped | GuardianFactId::LaunchMemoryAllocationLow => {
            GuardianDomain::Launch
        }
        GuardianFactId::LaunchResourceMemoryPressure
        | GuardianFactId::LaunchResourceCpuPressure
        | GuardianFactId::LaunchResourceInstallPressure => GuardianDomain::Performance,
        GuardianFactId::LaunchResourceDiskPressure => GuardianDomain::Filesystem,
        GuardianFactId::CustomJavaOverridePresent => GuardianDomain::Runtime,
        GuardianFactId::CustomJvmPresetPresent | GuardianFactId::CustomJvmArgsPresent => {
            GuardianDomain::Jvm
        }
        GuardianFactId::LauncherManagedArtifactSignatureCorruption => GuardianDomain::Download,
        GuardianFactId::ArtifactChecksumMismatch
        | GuardianFactId::ArtifactSizeMismatch
        | GuardianFactId::ArtifactMissing => GuardianDomain::Library,
        GuardianFactId::ManagedFileCorrupt => GuardianDomain::Unknown,
        GuardianFactId::ProviderDataInvalid => GuardianDomain::Network,
        GuardianFactId::PrimitiveRefused | GuardianFactId::OwnershipUnknown => {
            GuardianDomain::Unknown
        }
        _ => match node.domain {
            DomainTemplate::Static(domain) => domain,
            DomainTemplate::FactDomain => {
                panic!("missing producer domain for {}", fact_id.as_str())
            }
        },
    }
}

fn producer_fact_reliability(fact_id: GuardianFactId) -> FactReliability {
    match fact_id {
        GuardianFactId::JavaProbeFailed
        | GuardianFactId::JavaMajorMismatch
        | GuardianFactId::JavaUpdateTooOld
        | GuardianFactId::ManagedRuntimeRosettaRequired
        | GuardianFactId::ManagedRuntimeUnavailableForPlatform
        | GuardianFactId::ArtifactChecksumMismatch
        | GuardianFactId::ArtifactSizeMismatch => FactReliability::ValidatedProbe,
        GuardianFactId::JavaOverrideEmpty
        | GuardianFactId::JavaOverrideUndefinedSentinel
        | GuardianFactId::JvmArgsParseFailed
        | GuardianFactId::JvmArgReservedLauncherFlag
        | GuardianFactId::JvmArgMemoryConflict
        | GuardianFactId::JvmArgUnsupportedGc
        | GuardianFactId::JvmArgUnlockOrderInvalid
        | GuardianFactId::JvmArgUnsafeClasspathOverride
        | GuardianFactId::JvmArgUnsafeNativePathOverride
        | GuardianFactId::JvmArgAgentOverride
        | GuardianFactId::LauncherManagedArtifactSignatureCorruption => {
            FactReliability::ExactClassifier
        }
        GuardianFactId::JavaOverrideMissing
        | GuardianFactId::ManagedRuntimeMissing
        | GuardianFactId::ManagedRuntimeReadyMarkerMissing
        | GuardianFactId::VersionJsonMissing
        | GuardianFactId::ParentVersionMissing
        | GuardianFactId::ClientJarMissing
        | GuardianFactId::LibrariesMissing
        | GuardianFactId::AssetIndexMissing => FactReliability::ExpectedMarkerAbsence,
        id if FactRequirement::ProcessLifecycle
            .source_fact_ids()
            .contains(&id) =>
        {
            FactReliability::ProcessLifecycle
        }
        _ => FactReliability::DirectStructured,
    }
}

fn producer_fact_overrides(
    fact_id: GuardianFactId,
) -> (Option<GuardianSeverity>, Option<GuardianConfidence>) {
    match fact_id {
        GuardianFactId::ManagedRuntimeMissing => (
            Some(GuardianSeverity::Recoverable),
            Some(GuardianConfidence::Confirmed),
        ),
        GuardianFactId::VersionJsonMissing
        | GuardianFactId::ParentVersionMissing
        | GuardianFactId::IncompleteInstall
        | GuardianFactId::ClientJarMissing
        | GuardianFactId::LibrariesMissing
        | GuardianFactId::AssetIndexMissing => (
            Some(GuardianSeverity::Blocking),
            Some(GuardianConfidence::Confirmed),
        ),
        GuardianFactId::LaunchMemoryMinClamped
        | GuardianFactId::LaunchMemoryAllocationLow
        | GuardianFactId::LaunchResourceMemoryPressure
        | GuardianFactId::LaunchResourceCpuPressure
        | GuardianFactId::LaunchResourceInstallPressure
        | GuardianFactId::LaunchResourceDiskPressure
        | GuardianFactId::CustomJavaOverridePresent
        | GuardianFactId::CustomJvmPresetPresent
        | GuardianFactId::CustomJvmArgsPresent
        | GuardianFactId::PerformanceFallbackSelected
        | GuardianFactId::PerformanceHealthFallback => (
            Some(GuardianSeverity::Warning),
            Some(GuardianConfidence::High),
        ),
        GuardianFactId::PerformanceRulesInvalid => (
            Some(GuardianSeverity::Degraded),
            Some(GuardianConfidence::Confirmed),
        ),
        GuardianFactId::PerformanceHealthDegraded
        | GuardianFactId::PerformanceRepeatedFailureMemory => (
            Some(GuardianSeverity::Degraded),
            Some(GuardianConfidence::High),
        ),
        GuardianFactId::PerformanceHealthInvalid | GuardianFactId::PerformanceUserOwnedConflict => {
            (
                Some(GuardianSeverity::Blocking),
                Some(GuardianConfidence::Confirmed),
            )
        }
        _ => (None, None),
    }
}

fn target_kind_for_domain(domain: GuardianDomain) -> TargetKind {
    match domain {
        GuardianDomain::Config | GuardianDomain::Jvm | GuardianDomain::State => TargetKind::Config,
        GuardianDomain::Library | GuardianDomain::Install => TargetKind::Artifact,
        GuardianDomain::Runtime => TargetKind::Runtime,
        GuardianDomain::Download | GuardianDomain::Network => TargetKind::NetworkResource,
        GuardianDomain::Performance => TargetKind::PerformanceComposition,
        GuardianDomain::Launch | GuardianDomain::Startup | GuardianDomain::Session => {
            TargetKind::Session
        }
        GuardianDomain::Filesystem => TargetKind::FilesystemPath,
        GuardianDomain::Auth => TargetKind::Account,
        GuardianDomain::Unknown => TargetKind::Config,
    }
}

fn assert_snapshot_coverage(snapshot: &GuardianDecisionSnapshot) {
    assert_eq!(snapshot.contexts, policy_contexts());
    assert_eq!(
        snapshot.source_cases.len(),
        GRAPH_SOURCE_COUNT + UNKNOWN_SOURCE_COUNT
    );
    assert_eq!(RAW_DIAGNOSIS_CASE_COUNT, 1_372);
    assert_eq!(RAW_POLICY_EVALUATION_COUNT, 65_856);
    assert_eq!(COMPRESSED_POLICY_CELL_COUNT, 17_136);
    assert!(
        snapshot
            .source_cases
            .windows(2)
            .all(|cases| cases[0].id < cases[1].id)
    );

    let mut case_ids = HashSet::new();
    let mut graph_diagnosis_ids = HashSet::new();
    let mut graph_sources = 0;
    let mut unknown_sources = 0;
    let mut graph_source_phases = 0;
    let mut raw_diagnosis_cases = 0;
    for case in &snapshot.source_cases {
        assert!(case_ids.insert(case.id.as_str()), "duplicate {}", case.id);
        assert!(!case.allowed_phases.is_empty(), "{}", case.id);
        match case.input {
            SourceInput::Fact { fact_id, .. } => {
                graph_sources += 1;
                graph_source_phases += case.allowed_phases.len();
                raw_diagnosis_cases += case.allowed_phases.len() * case.ownership_profiles.len();
                graph_diagnosis_ids.insert(case.diagnosis.id);
                assert_eq!(case.diagnosis.fact_ids, vec![fact_id], "{}", case.id);
                assert_eq!(
                    case.ownership_profiles
                        .iter()
                        .map(|profile| profile.ownership)
                        .collect::<Vec<_>>(),
                    OWNERSHIPS,
                    "{}",
                    case.id
                );
            }
            SourceInput::Empty => {
                unknown_sources += 1;
                raw_diagnosis_cases += 1;
                assert_eq!(case.allowed_phases.len(), 1, "{}", case.id);
                assert_eq!(case.ownership_profiles.len(), 1, "{}", case.id);
                assert_eq!(
                    case.ownership_profiles[0].ownership,
                    OwnershipClass::Unknown,
                    "{}",
                    case.id
                );
            }
        }
    }
    assert_eq!(graph_sources, GRAPH_SOURCE_COUNT);
    assert_eq!(graph_source_phases, GRAPH_SOURCE_PHASE_COUNT);
    assert_eq!(unknown_sources, UNKNOWN_SOURCE_COUNT);
    assert_eq!(raw_diagnosis_cases, RAW_DIAGNOSIS_CASE_COUNT);
    assert_eq!(graph_diagnosis_ids.len(), GRAPH_DIAGNOSIS_COUNT);

    let profile_ids = snapshot
        .policy_profiles
        .iter()
        .map(|profile| profile.id.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(profile_ids.len(), snapshot.policy_profiles.len());
    let mut saw_planned_block = false;
    let mut saw_unplanned_block = false;
    for profile in &snapshot.policy_profiles {
        assert_eq!(profile.id, policy_profile_id(&profile.modes));
        assert_eq!(
            profile.modes.iter().map(|row| row.mode).collect::<Vec<_>>(),
            MODES
        );
        assert!(
            profile
                .modes
                .iter()
                .all(|row| row.contexts.len() == CONTEXT_COUNT)
        );
        assert!(
            profile
                .modes
                .iter()
                .flat_map(|row| &row.contexts)
                .all(|cell| cell.plan_integrity),
            "{}",
            profile.id
        );
        for cell in profile.modes.iter().flat_map(|row| &row.contexts) {
            if cell.decision_kind == GuardianActionKind::Block {
                saw_planned_block |= cell.plan_present;
                saw_unplanned_block |= !cell.plan_present;
            }
        }
    }
    assert!(saw_planned_block);
    assert!(saw_unplanned_block);
    for source in &snapshot.source_cases {
        for profile in &source.ownership_profiles {
            assert!(
                profile_ids.contains(profile.policy_profile.as_str()),
                "{} references {}",
                source.id,
                profile.policy_profile
            );
        }
    }
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-decision-snapshot-v1.json")
}
