use super::rules::{DIAGNOSIS_RULES, DecisionPriorityBand, DiagnosisRule};
use super::{
    ActionPlanPrerequisite, DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianSeverity, SafetyCase,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Diagnosis {
    id: DiagnosisId,
    domain: GuardianDomain,
    severity: GuardianSeverity,
    confidence: GuardianConfidence,
    ownership: OwnershipClass,
    phase: OperationPhase,
    fact_ids: Vec<GuardianFactId>,
    affected_targets: Vec<TargetDescriptor>,
    candidate_actions: Vec<GuardianActionKind>,
    public_reason_template: String,
    #[serde(skip)]
    priority: DecisionPriorityBand,
}

impl Diagnosis {
    pub fn id(&self) -> DiagnosisId {
        self.id
    }

    pub fn domain(&self) -> GuardianDomain {
        self.domain
    }

    pub fn severity(&self) -> GuardianSeverity {
        self.severity
    }

    pub fn confidence(&self) -> GuardianConfidence {
        self.confidence
    }

    pub fn ownership(&self) -> OwnershipClass {
        self.ownership
    }

    pub fn phase(&self) -> OperationPhase {
        self.phase
    }

    pub fn fact_ids(&self) -> &[GuardianFactId] {
        &self.fact_ids
    }

    pub fn affected_targets(&self) -> &[TargetDescriptor] {
        &self.affected_targets
    }

    pub fn candidate_actions(&self) -> &[GuardianActionKind] {
        &self.candidate_actions
    }

    pub fn public_reason_template(&self) -> &str {
        &self.public_reason_template
    }

    pub(super) fn priority(&self) -> DecisionPriorityBand {
        self.priority
    }

    pub fn action_prerequisite(&self) -> ActionPlanPrerequisite {
        ActionPlanPrerequisite {
            diagnosis_id: self.id,
            ownership: self.ownership,
            confidence: self.confidence,
            affected_targets: self.affected_targets.clone(),
            candidate_actions: self.candidate_actions.clone(),
        }
    }
}

pub fn diagnose(facts: &[GuardianFact], phase: OperationPhase) -> Vec<Diagnosis> {
    let mut diagnoses = DIAGNOSIS_RULES
        .iter()
        .enumerate()
        .filter_map(|(rule_index, rule)| {
            diagnosis_for_rule(rule, facts, phase)
                .map(|(first_fact_index, diagnosis)| (first_fact_index, rule_index, diagnosis))
        })
        .collect::<Vec<_>>();
    if diagnoses.is_empty() {
        return vec![unknown_diagnosis(facts, phase)];
    }
    diagnoses.sort_by_key(|(first_fact_index, rule_index, _)| (*first_fact_index, *rule_index));
    diagnoses
        .into_iter()
        .map(|(_, _, diagnosis)| diagnosis)
        .collect()
}

pub fn build_safety_case(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    facts: &[GuardianFact],
) -> SafetyCase {
    SafetyCase {
        operation_id,
        mode,
        phase,
        diagnoses: diagnose(facts, phase),
    }
}

fn diagnosis_for_rule(
    rule: &DiagnosisRule,
    facts: &[GuardianFact],
    phase: OperationPhase,
) -> Option<(usize, Diagnosis)> {
    if !rule.matches(facts, phase) {
        return None;
    }
    let first_fact_index = facts
        .iter()
        .position(|fact| rule.trigger_matches(fact, phase))?;
    let supporting_facts = rule
        .trigger_fact_ids
        .iter()
        .flat_map(|fact_id| {
            facts
                .iter()
                .filter(move |fact| fact.id == *fact_id && rule.trigger_matches(fact, phase))
        })
        .collect::<Vec<_>>();
    let ownership = conservative_ownership(&supporting_facts);
    let resolved = rule.resolve(&supporting_facts, facts, phase);
    let affected_target_facts = if resolved.priority
        == DecisionPriorityBand::RegisteredArtifactRepair
    {
        facts
            .iter()
            .filter(|fact| {
                fact.id == GuardianFactId::RegisteredArtifactRepairAvailable && fact.phase == phase
            })
            .collect::<Vec<_>>()
    } else {
        supporting_facts.clone()
    };
    let evidence_facts = resolved
        .evidence_fact_ids
        .iter()
        .flat_map(|fact_id| facts.iter().filter(move |fact| fact.id == *fact_id))
        .collect::<Vec<_>>();

    Some((
        first_fact_index,
        Diagnosis {
            id: rule.id,
            domain: rule.domain(&supporting_facts),
            severity: resolved.severity,
            confidence: resolved.confidence,
            ownership,
            phase,
            fact_ids: resolved
                .evidence_fact_ids
                .iter()
                .copied()
                .filter(|fact_id| evidence_facts.iter().any(|fact| fact.id == *fact_id))
                .collect(),
            affected_targets: affected_targets_for_rule(
                &affected_target_facts,
                rule.domain(&supporting_facts),
                ownership,
                phase,
            ),
            candidate_actions: resolved.candidate_actions.to_vec(),
            public_reason_template: rule.public_reason_template.to_string(),
            priority: resolved.priority,
        },
    ))
}

fn unknown_diagnosis(facts: &[GuardianFact], phase: OperationPhase) -> Diagnosis {
    let evidence_facts = facts
        .iter()
        .filter(|fact| !is_condition_fact(fact.id))
        .collect::<Vec<_>>();
    let ownership = conservative_ownership(&evidence_facts);
    let mut affected_targets =
        distinct_sorted_targets(evidence_facts.iter().filter_map(|fact| fact.target.clone()));
    if affected_targets.is_empty() {
        affected_targets.push(fallback_target(GuardianDomain::Unknown, ownership, phase));
    }
    Diagnosis {
        id: DiagnosisId::UnknownFailure(phase),
        domain: GuardianDomain::Unknown,
        severity: GuardianSeverity::Warning,
        confidence: GuardianConfidence::Low,
        ownership,
        phase,
        fact_ids: supporting_fact_ids(&evidence_facts, phase),
        affected_targets,
        candidate_actions: vec![
            GuardianActionKind::RecordOnly,
            GuardianActionKind::Warn,
            GuardianActionKind::AskUser,
        ],
        public_reason_template: "unknown_failure".to_string(),
        priority: DecisionPriorityBand::UnknownLow,
    }
}

fn affected_targets_for_rule(
    supporting_facts: &[&GuardianFact],
    domain: GuardianDomain,
    ownership: OwnershipClass,
    phase: OperationPhase,
) -> Vec<TargetDescriptor> {
    let targets = distinct_sorted_targets(
        supporting_facts
            .iter()
            .filter_map(|fact| fact.target.clone()),
    );
    if targets.is_empty() {
        vec![fallback_target(domain, ownership, phase)]
    } else {
        targets
    }
}

fn supporting_fact_ids(facts: &[&GuardianFact], phase: OperationPhase) -> Vec<GuardianFactId> {
    let mut seen = HashSet::new();
    let mut fact_ids = facts
        .iter()
        .map(|fact| fact.id)
        .filter(|fact_id| seen.insert(*fact_id))
        .collect::<Vec<_>>();
    if fact_ids.is_empty() {
        fact_ids.push(GuardianFactId::NoStructuredFact(phase));
    }
    fact_ids
}

fn is_condition_fact(id: GuardianFactId) -> bool {
    matches!(
        id,
        GuardianFactId::LaunchFailureClassified
            | GuardianFactId::LaunchRuntimeFallbackAvailable
            | GuardianFactId::LaunchJvmStripAvailable
            | GuardianFactId::LaunchJvmPresetDowngradeAvailable
            | GuardianFactId::RegisteredArtifactRepairAvailable
    )
}

fn distinct_sorted_targets(
    targets: impl IntoIterator<Item = TargetDescriptor>,
) -> Vec<TargetDescriptor> {
    let mut targets = targets
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    targets.sort_by(compare_targets);
    targets
}

fn compare_targets(left: &TargetDescriptor, right: &TargetDescriptor) -> Ordering {
    system_rank(left.system)
        .cmp(&system_rank(right.system))
        .then_with(|| target_kind_rank(left.kind).cmp(&target_kind_rank(right.kind)))
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| ownership_rank(left.ownership).cmp(&ownership_rank(right.ownership)))
}

fn system_rank(system: StabilizationSystem) -> u8 {
    match system {
        StabilizationSystem::Application => 0,
        StabilizationSystem::Execution => 1,
        StabilizationSystem::Guardian => 2,
        StabilizationSystem::Performance => 3,
        StabilizationSystem::Observability => 4,
        StabilizationSystem::State => 5,
        StabilizationSystem::Interface => 6,
    }
}

fn target_kind_rank(kind: TargetKind) -> u8 {
    match kind {
        TargetKind::Instance => 0,
        TargetKind::Version => 1,
        TargetKind::Artifact => 2,
        TargetKind::Runtime => 3,
        TargetKind::Session => 4,
        TargetKind::Account => 5,
        TargetKind::Config => 6,
        TargetKind::PerformanceComposition => 7,
        TargetKind::FilesystemPath => 8,
        TargetKind::NetworkResource => 9,
    }
}

fn ownership_rank(ownership: OwnershipClass) -> u8 {
    match ownership {
        OwnershipClass::LauncherManaged => 0,
        OwnershipClass::CompositionManaged => 1,
        OwnershipClass::ExternalProviderDerived => 2,
        OwnershipClass::UserOwned => 3,
        OwnershipClass::Unknown => 4,
    }
}

fn conservative_ownership(facts: &[&GuardianFact]) -> OwnershipClass {
    facts
        .iter()
        .map(|fact| fact.ownership)
        .max_by_key(|ownership| ownership_rank(*ownership))
        .unwrap_or(OwnershipClass::Unknown)
}

fn fallback_target(
    domain: GuardianDomain,
    ownership: OwnershipClass,
    phase: OperationPhase,
) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        target_kind_for_domain(domain),
        format!("guardian-{}-{}", domain_name(domain), phase_name(phase)),
        ownership,
    )
}

fn target_kind_for_domain(domain: GuardianDomain) -> TargetKind {
    match domain {
        GuardianDomain::Runtime => TargetKind::Runtime,
        GuardianDomain::Download | GuardianDomain::Network => TargetKind::NetworkResource,
        GuardianDomain::Library | GuardianDomain::Filesystem | GuardianDomain::Install => {
            TargetKind::Artifact
        }
        GuardianDomain::Launch | GuardianDomain::Startup | GuardianDomain::Session => {
            TargetKind::Session
        }
        GuardianDomain::Auth => TargetKind::Account,
        GuardianDomain::Performance => TargetKind::PerformanceComposition,
        GuardianDomain::Config
        | GuardianDomain::Jvm
        | GuardianDomain::State
        | GuardianDomain::Unknown => TargetKind::Config,
    }
}

fn phase_name(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::Startup => "startup",
        OperationPhase::Planning => "planning",
        OperationPhase::Validating => "validating",
        OperationPhase::Downloading => "downloading",
        OperationPhase::Installing => "installing",
        OperationPhase::Preparing => "preparing",
        OperationPhase::Launching => "launching",
        OperationPhase::Running => "running",
        OperationPhase::Repairing => "repairing",
        OperationPhase::RollingBack => "rolling_back",
        OperationPhase::Completed => "completed",
        OperationPhase::Failed => "failed",
    }
}

fn domain_name(domain: GuardianDomain) -> &'static str {
    match domain {
        GuardianDomain::Config => "config",
        GuardianDomain::Library => "library",
        GuardianDomain::Runtime => "runtime",
        GuardianDomain::Jvm => "jvm",
        GuardianDomain::Install => "install",
        GuardianDomain::Download => "download",
        GuardianDomain::Performance => "performance",
        GuardianDomain::Launch => "launch",
        GuardianDomain::Startup => "startup",
        GuardianDomain::Session => "session",
        GuardianDomain::Filesystem => "filesystem",
        GuardianDomain::Network => "network",
        GuardianDomain::Auth => "auth",
        GuardianDomain::State => "state",
        GuardianDomain::Unknown => "unknown",
    }
}
