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
const FACT_SOURCE_COUNT: usize = 69;
const DIAGNOSIS_COUNT: usize = 46;
const FACT_SOURCE_PHASE_COUNT: usize = 272;
const UNKNOWN_SOURCE_COUNT: usize = 12;
const CONTEXT_COUNT: usize = 16;
const FACT_SOURCE_OWNERSHIP_COUNT: usize = 5;
const MODE_COUNT: usize = 3;
const POLICY_CELLS_PER_ROOT: usize = CONTEXT_COUNT * MODE_COUNT;
const POLICY_ROOT_COUNT: usize =
    FACT_SOURCE_COUNT * FACT_SOURCE_OWNERSHIP_COUNT + UNKNOWN_SOURCE_COUNT;
const RAW_DIAGNOSIS_CASE_COUNT: usize =
    FACT_SOURCE_PHASE_COUNT * FACT_SOURCE_OWNERSHIP_COUNT + UNKNOWN_SOURCE_COUNT;
const RAW_POLICY_EVALUATION_COUNT: usize = RAW_DIAGNOSIS_CASE_COUNT * POLICY_CELLS_PER_ROOT;
const COMPRESSED_POLICY_CELL_COUNT: usize = POLICY_ROOT_COUNT * POLICY_CELLS_PER_ROOT;

const MODES: [GuardianMode; MODE_COUNT] = [
    GuardianMode::Managed,
    GuardianMode::Custom,
    GuardianMode::Disabled,
];
const OWNERSHIPS: [OwnershipClass; FACT_SOURCE_OWNERSHIP_COUNT] = [
    OwnershipClass::LauncherManaged,
    OwnershipClass::CompositionManaged,
    OwnershipClass::UserOwned,
    OwnershipClass::ExternalProviderDerived,
    OwnershipClass::Unknown,
];
const CONTEXT_IDS: [&str; CONTEXT_COUNT] = [
    "j0-s0-r0-u0",
    "j1-s0-r0-u0",
    "j0-s1-r0-u0",
    "j1-s1-r0-u0",
    "j0-s0-r1-u0",
    "j1-s0-r1-u0",
    "j0-s1-r1-u0",
    "j1-s1-r1-u0",
    "j0-s0-r0-u1",
    "j1-s0-r0-u1",
    "j0-s1-r0-u1",
    "j1-s1-r0-u1",
    "j0-s0-r1-u1",
    "j1-s0-r1-u1",
    "j0-s1-r1-u1",
    "j1-s1-r1-u1",
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
    assert_snapshot_coverage(&fixture);
    let replayed = replay_snapshot(&fixture);

    assert_eq!(fixture, replayed);
    let pretty = serde_json::to_string_pretty(&replayed).expect("serialize decision snapshot");
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
    let committed = serde_json::from_str::<GuardianDecisionSnapshot>(SNAPSHOT_FIXTURE)
        .expect("strict Guardian decision snapshot fixture");
    assert_snapshot_coverage(&committed);
    let replayed = replay_snapshot(&committed);
    assert_snapshot_coverage(&replayed);
    let fixture = serde_json::to_string_pretty(&replayed)
        .expect("serialize regenerated Guardian decision snapshot");
    std::fs::write(snapshot_fixture_path(), format!("{fixture}\n"))
        .expect("write regenerated Guardian decision snapshot");
}

fn replay_snapshot(committed: &GuardianDecisionSnapshot) -> GuardianDecisionSnapshot {
    let contexts = committed.contexts.clone();
    let mut matrices = BTreeMap::<String, Vec<ModePolicyRow>>::new();
    let source_cases = committed
        .source_cases
        .iter()
        .map(|source| replay_source_case(source, &contexts, &mut matrices))
        .collect();

    let policy_profiles = matrices
        .into_iter()
        .map(|(id, modes)| PolicyProfile { id, modes })
        .collect();

    GuardianDecisionSnapshot {
        schema: committed.schema,
        contexts,
        source_cases,
        policy_profiles,
    }
}

fn replay_source_case(
    committed: &SourceCase,
    contexts: &[PolicyContextCoordinate],
    matrices: &mut BTreeMap<String, Vec<ModePolicyRow>>,
) -> SourceCase {
    let mut expected_projection = None;
    let mut ownership_profiles = Vec::with_capacity(committed.ownership_profiles.len());

    for committed_profile in &committed.ownership_profiles {
        let ownership = committed_profile.ownership;
        let mut expected_matrix = None;
        for phase in &committed.allowed_phases {
            let diagnosis = replay_source_diagnosis(&committed.input, *phase, ownership);
            assert_eq!(diagnosis.phase, *phase, "{} at {phase:?}", committed.id);
            assert_eq!(
                diagnosis.ownership, ownership,
                "{} at {phase:?}",
                committed.id
            );
            let projection = DiagnosisProjection::from(&diagnosis);
            if let Some(expected) = &expected_projection {
                assert_eq!(expected, &projection, "{} at {phase:?}", committed.id);
            } else {
                expected_projection = Some(projection);
            }

            let matrix = policy_matrix(&diagnosis, contexts);
            if let Some(expected) = &expected_matrix {
                assert_eq!(expected, &matrix, "{} at {phase:?}", committed.id);
            } else {
                expected_matrix = Some(matrix);
            }
        }

        let matrix = expected_matrix.expect("snapshot source has an allowed phase");
        let policy_profile = insert_policy_profile(matrices, matrix);
        ownership_profiles.push(OwnershipProfileRef {
            ownership,
            policy_profile,
        });
    }

    let diagnosis = expected_projection.expect("snapshot source has a diagnosis");
    SourceCase {
        id: committed.id.clone(),
        input: committed.input.clone(),
        allowed_phases: committed.allowed_phases.clone(),
        diagnosis,
        ownership_profiles,
    }
}

fn replay_source_diagnosis(
    input: &SourceInput,
    phase: OperationPhase,
    ownership: OwnershipClass,
) -> Diagnosis {
    let facts = match input {
        SourceInput::Fact {
            fact_id,
            domain,
            reliability,
            severity,
            confidence,
        } => vec![GuardianFact {
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
        }],
        SourceInput::Empty => Vec::new(),
    };
    let mut diagnoses = diagnose_facts(&facts, phase);
    assert_eq!(diagnoses.len(), 1, "snapshot source at {phase:?}");
    diagnoses
        .pop()
        .expect("snapshot source produces one diagnosis")
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
    assert_context_coverage(&snapshot.contexts);
    assert_eq!(
        snapshot.source_cases.len(),
        FACT_SOURCE_COUNT + UNKNOWN_SOURCE_COUNT
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
    let mut diagnosis_ids = HashSet::new();
    let mut fact_ids = HashSet::new();
    let mut fact_sources = 0;
    let mut unknown_sources = 0;
    let mut fact_source_phases = 0;
    let mut unknown_phases = HashSet::new();
    let mut raw_diagnosis_cases = 0;
    for case in &snapshot.source_cases {
        assert!(case_ids.insert(case.id.as_str()), "duplicate {}", case.id);
        assert!(!case.allowed_phases.is_empty(), "{}", case.id);
        assert_eq!(
            case.allowed_phases
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len(),
            case.allowed_phases.len(),
            "duplicate allowed phase in {}",
            case.id
        );
        match case.input {
            SourceInput::Fact { fact_id, .. } => {
                fact_sources += 1;
                fact_source_phases += case.allowed_phases.len();
                raw_diagnosis_cases += case.allowed_phases.len() * case.ownership_profiles.len();
                assert!(
                    fact_ids.insert(fact_id),
                    "duplicate fact source {fact_id:?}"
                );
                diagnosis_ids.insert(case.diagnosis.id);
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
                assert!(unknown_phases.insert(case.allowed_phases[0]), "{}", case.id);
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
    assert_eq!(fact_sources, FACT_SOURCE_COUNT);
    assert_eq!(fact_source_phases, FACT_SOURCE_PHASE_COUNT);
    assert_eq!(unknown_sources, UNKNOWN_SOURCE_COUNT);
    assert_eq!(unknown_phases, PHASES.into_iter().collect());
    assert_eq!(raw_diagnosis_cases, RAW_DIAGNOSIS_CASE_COUNT);
    assert_eq!(diagnosis_ids.len(), DIAGNOSIS_COUNT);

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
    let referenced_profile_ids = snapshot
        .source_cases
        .iter()
        .flat_map(|source| &source.ownership_profiles)
        .map(|profile| profile.policy_profile.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(referenced_profile_ids, profile_ids);
}

fn assert_context_coverage(contexts: &[PolicyContextCoordinate]) {
    assert_eq!(contexts.len(), CONTEXT_COUNT);
    let mut ids = HashSet::new();
    let mut coordinates = HashSet::new();
    for context in contexts {
        let bits = u8::from(context.journal_available)
            | (u8::from(context.suppression_active) << 1)
            | (u8::from(context.public_redaction_ready) << 2)
            | (u8::from(context.explicit_user_intent) << 3);
        assert_eq!(context.id, CONTEXT_IDS[usize::from(bits)]);
        assert!(ids.insert(context.id.as_str()), "duplicate context id");
        assert!(coordinates.insert(bits), "duplicate policy context");
    }
    assert_eq!(coordinates, (0_u8..CONTEXT_COUNT as u8).collect());
}

fn snapshot_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-decision-snapshot-v1.json")
}
