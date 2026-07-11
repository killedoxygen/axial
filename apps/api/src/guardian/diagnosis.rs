use super::inference_graph::{AffectedTargetStrategy, diagnosis_node_for_fact};
use super::{
    Diagnosis, DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianImpactVector, GuardianMode, GuardianSeverity, SafetyCase,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};

pub fn diagnose_facts(facts: &[GuardianFact], phase: OperationPhase) -> Vec<Diagnosis> {
    let mut diagnoses = facts
        .iter()
        .filter_map(|fact| diagnosis_for_fact(facts, fact, phase))
        .collect::<Vec<_>>();
    if diagnoses.is_empty() {
        diagnoses.push(unknown_diagnosis(facts, phase));
    }
    diagnoses
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
        diagnoses: diagnose_facts(facts, phase),
    }
}

fn diagnosis_for_fact(
    facts: &[GuardianFact],
    fact: &GuardianFact,
    phase: OperationPhase,
) -> Option<Diagnosis> {
    let node = diagnosis_node_for_fact(fact)?;
    let evaluation = node.evaluate(facts, fact, phase);
    if !evaluation.selected_for_diagnosis() {
        return None;
    }

    Some(Diagnosis {
        id: DiagnosisId::new(node.diagnosis_id(fact)),
        domain: node.domain(fact),
        severity: evaluation.resolved_severity,
        confidence: evaluation.resolved_confidence,
        ownership: fact.ownership,
        phase,
        fact_ids: vec![fact.id],
        affected_targets: affected_targets_for_fact(fact, phase, node.target_strategy),
        impact: evaluation.impact,
        candidate_actions: node.candidate_actions.to_vec(),
        public_reason_template: node.public_reason_template(fact),
    })
}

fn unknown_diagnosis(facts: &[GuardianFact], phase: OperationPhase) -> Diagnosis {
    let ownership = facts
        .first()
        .map(|fact| fact.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    let mut affected_targets = facts
        .iter()
        .filter_map(|fact| fact.target.clone())
        .collect::<Vec<_>>();
    if affected_targets.is_empty() {
        affected_targets.push(fallback_target(GuardianDomain::Unknown, ownership, phase));
    }
    Diagnosis {
        id: DiagnosisId::new(format!("unknown_failure_{}", phase_name(phase))),
        domain: GuardianDomain::Unknown,
        severity: GuardianSeverity::Warning,
        confidence: GuardianConfidence::Low,
        ownership,
        phase,
        fact_ids: supporting_fact_ids(facts, phase),
        affected_targets,
        impact: GuardianImpactVector::record_only(),
        candidate_actions: vec![
            GuardianActionKind::RecordOnly,
            GuardianActionKind::Warn,
            GuardianActionKind::AskUser,
        ],
        public_reason_template: "unknown_failure".to_string(),
    }
}

fn affected_targets_for_fact(
    fact: &GuardianFact,
    phase: OperationPhase,
    strategy: AffectedTargetStrategy,
) -> Vec<TargetDescriptor> {
    match strategy {
        AffectedTargetStrategy::FactTargetOrGuardianFallback => fact
            .target
            .clone()
            .into_iter()
            .chain(
                fact.target
                    .is_none()
                    .then(|| fallback_target(fact.domain, fact.ownership, phase)),
            )
            .collect(),
    }
}

fn supporting_fact_ids(facts: &[GuardianFact], phase: OperationPhase) -> Vec<GuardianFactId> {
    let mut fact_ids = facts.iter().map(|fact| fact.id).collect::<Vec<_>>();
    if fact_ids.is_empty() {
        fact_ids.push(GuardianFactId::NoStructuredFact(phase));
    }
    fact_ids
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
