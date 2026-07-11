use super::{
    Diagnosis, FactReliability, GuardianActionKind, GuardianConfidence, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianImpactVector, GuardianSeverity,
};
use crate::state::contracts::{OperationPhase, OwnershipClass};

const BASE_CONFIDENCE_SCORE: f32 = 0.10;
const MISSING_REQUIRED_FACT_PENALTY: f32 = 0.40;
const PHASE_MISMATCH_PENALTY: f32 = 0.30;
const UNKNOWN_DIAGNOSIS_THRESHOLD: f32 = 0.45;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FactRequirement {
    Any(&'static [GuardianFactId]),
    ProcessLifecycle,
}

impl FactRequirement {
    pub(super) fn matches_fact_id(self, fact_id: GuardianFactId) -> bool {
        match self {
            Self::Any(fact_ids) => fact_ids.contains(&fact_id),
            Self::ProcessLifecycle => is_process_fact(fact_id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WeightedFact {
    pub(super) fact_id: GuardianFactId,
    pub(super) weight: f32,
}

impl WeightedFact {
    const fn new(fact_id: GuardianFactId, weight: f32) -> Self {
        Self { fact_id, weight }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiagnosisIdTemplate {
    Static(&'static str),
    FactId,
    Readiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DomainTemplate {
    Static(GuardianDomain),
    FactDomain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SeverityTemplate {
    Static(GuardianSeverity),
    FactOr(GuardianSeverity),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfidenceTemplate {
    Static(GuardianConfidence),
    FactOr(GuardianConfidence),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ImpactTemplate {
    LaunchBlocking,
    RepairableCorruption,
    RecordOnly,
    JavaOverrideUnavailable,
    ManagedRuntimeMissing,
    Readiness,
    ResourcePressure,
    CustomIntent,
    JvmArgsMalformed,
    UnsafeJvmOverride,
    ArtifactOwnershipUnsafe,
    PerformanceRulesInvalid,
    PerformanceHealth,
    PerformanceFallback,
    PerformanceRepeatedFailure,
    PerformanceUserOwnedConflict,
    PersistedStateSchemaInvalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AffectedTargetStrategy {
    FactTargetOrGuardianFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReasonTemplate {
    Static(&'static str),
    FactId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OwnershipRequirement {
    None,
    Classified,
    LauncherManaged,
    CompositionManaged,
    UserOrUnknownProtected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JournalRequirement {
    None,
    RequiredForAttemptAction,
    RequiredForManagedMutation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RedactionRequirement {
    PublicOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RetryLoopSensitivity {
    None,
    OneAttemptOverride,
    RepairAttempt,
    ProviderRetry,
    RepeatedFailureMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DestructiveMutationRisk {
    None,
    ManagedMutation,
    UserOrUnknownProtected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UserIntentSensitivity {
    None,
    ExplicitTechnicalIntent,
    PerformanceComposition,
    UserDataBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ActionEligibility {
    pub(super) ownership_requirement: OwnershipRequirement,
    pub(super) journal_requirement: JournalRequirement,
    pub(super) redaction_requirement: RedactionRequirement,
    pub(super) retry_loop_sensitivity: RetryLoopSensitivity,
    pub(super) destructive_mutation_risk: DestructiveMutationRisk,
    pub(super) user_intent_sensitivity: UserIntentSensitivity,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct DiagnosisGraphNode {
    pub(super) id: DiagnosisIdTemplate,
    pub(super) domain: DomainTemplate,
    pub(super) required_facts: &'static [FactRequirement],
    pub(super) supporting_facts: &'static [WeightedFact],
    pub(super) contradicting_facts: &'static [WeightedFact],
    pub(super) phase_allowed: &'static [OperationPhase],
    pub(super) ownership_allowed: &'static [OwnershipClass],
    pub(super) severity: SeverityTemplate,
    pub(super) confidence: ConfidenceTemplate,
    pub(super) impact: ImpactTemplate,
    pub(super) eligibility: ActionEligibility,
    pub(super) target_strategy: AffectedTargetStrategy,
    pub(super) candidate_actions: &'static [GuardianActionKind],
    pub(super) public_reason_template: ReasonTemplate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct DiagnosisGraphEvaluation {
    pub(super) required_fact_satisfied: bool,
    pub(super) support_score: f32,
    pub(super) contradiction_score: f32,
    pub(super) phase_compatible: bool,
    pub(super) direct_fact_count: usize,
    pub(super) evidence_confidence_score: f32,
    pub(super) impact: GuardianImpactVector,
    pub(super) impact_scalar: f32,
    pub(super) resolved_severity: GuardianSeverity,
    pub(super) resolved_confidence: GuardianConfidence,
    pub(super) action_eligibility: ActionEligibility,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct DiagnosisGraphPolicyInput {
    pub(super) resolved_severity: GuardianSeverity,
    pub(super) resolved_confidence: GuardianConfidence,
    pub(super) impact_scalar: f32,
    pub(super) action_eligibility: ActionEligibility,
}

impl DiagnosisGraphEvaluation {
    pub(super) fn selected_for_diagnosis(self) -> bool {
        self.required_fact_satisfied
            || self.evidence_confidence_score >= UNKNOWN_DIAGNOSIS_THRESHOLD
    }
}

impl DiagnosisGraphNode {
    pub(super) fn matches_fact(&self, fact: &GuardianFact) -> bool {
        self.required_facts
            .iter()
            .any(|required| required.matches_fact_id(fact.id))
    }

    pub(super) fn diagnosis_id(&self, fact: &GuardianFact) -> String {
        match self.id {
            DiagnosisIdTemplate::Static(id) => id.to_string(),
            DiagnosisIdTemplate::FactId => fact.id.as_str().to_string(),
            DiagnosisIdTemplate::Readiness => readiness_diagnosis_id(fact.id)
                .expect("readiness graph must admit only mapped fact ids")
                .to_string(),
        }
    }

    pub(super) fn domain(&self, fact: &GuardianFact) -> GuardianDomain {
        match self.domain {
            DomainTemplate::Static(domain) => domain,
            DomainTemplate::FactDomain => fact.domain,
        }
    }

    pub(super) fn severity(&self, fact: &GuardianFact) -> GuardianSeverity {
        match self.severity {
            SeverityTemplate::Static(severity) => severity,
            SeverityTemplate::FactOr(default) => fact.severity.unwrap_or(default),
        }
    }

    pub(super) fn confidence(&self, fact: &GuardianFact) -> GuardianConfidence {
        match self.confidence {
            ConfidenceTemplate::Static(confidence) => confidence,
            ConfidenceTemplate::FactOr(default) => fact.confidence.unwrap_or(default),
        }
    }

    pub(super) fn impact(&self, fact: &GuardianFact) -> GuardianImpactVector {
        match self.impact {
            ImpactTemplate::LaunchBlocking => GuardianImpactVector::launch_blocking(),
            ImpactTemplate::RepairableCorruption => GuardianImpactVector::repairable_corruption(),
            ImpactTemplate::RecordOnly => GuardianImpactVector::record_only(),
            ImpactTemplate::JavaOverrideUnavailable => GuardianImpactVector {
                launchability_impact: 0.95,
                user_intent_impact: 0.65,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::ManagedRuntimeMissing => GuardianImpactVector {
                launchability_impact: 0.35,
                state_corruption_impact: 0.10,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::Readiness => GuardianImpactVector {
                launchability_impact: 0.95,
                state_corruption_impact: readiness_state_corruption_impact(fact.id),
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::ResourcePressure => GuardianImpactVector {
                launchability_impact: 0.35,
                performance_impact: 0.45,
                host_stability_impact: 0.50,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::CustomIntent => GuardianImpactVector {
                user_intent_impact: 0.55,
                launchability_impact: 0.20,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::JvmArgsMalformed => GuardianImpactVector {
                launchability_impact: 0.90,
                user_intent_impact: 0.70,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::UnsafeJvmOverride => GuardianImpactVector {
                launchability_impact: 0.90,
                user_intent_impact: 0.75,
                host_stability_impact: 0.60,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::ArtifactOwnershipUnsafe => GuardianImpactVector {
                data_loss_risk: 0.95,
                user_intent_impact: 0.80,
                launchability_impact: 0.70,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::PerformanceRulesInvalid => GuardianImpactVector {
                launchability_impact: 0.25,
                performance_impact: 0.80,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::PerformanceHealth => GuardianImpactVector {
                launchability_impact: 0.35,
                state_corruption_impact: if fact.id == GuardianFactId::PerformanceHealthInvalid {
                    0.75
                } else {
                    0.35
                },
                performance_impact: 0.80,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::PerformanceFallback => GuardianImpactVector {
                launchability_impact: 0.15,
                performance_impact: 0.60,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::PerformanceRepeatedFailure => GuardianImpactVector {
                launchability_impact: 0.30,
                performance_impact: 0.75,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::PerformanceUserOwnedConflict => GuardianImpactVector {
                data_loss_risk: 0.75,
                user_intent_impact: 0.85,
                performance_impact: 0.45,
                ..GuardianImpactVector::default()
            },
            ImpactTemplate::PersistedStateSchemaInvalid => GuardianImpactVector {
                launchability_impact: 0.25,
                state_corruption_impact: 0.80,
                ..GuardianImpactVector::default()
            },
        }
    }

    pub(super) fn public_reason_template(&self, fact: &GuardianFact) -> String {
        match self.public_reason_template {
            ReasonTemplate::Static(template) => template.to_string(),
            ReasonTemplate::FactId => fact.id.as_str().to_string(),
        }
    }

    pub(super) fn evaluate(
        &self,
        facts: &[GuardianFact],
        fact: &GuardianFact,
        phase: OperationPhase,
    ) -> DiagnosisGraphEvaluation {
        let required_fact_satisfied = self
            .required_facts
            .iter()
            .all(|required| facts.iter().any(|fact| required.matches_fact_id(fact.id)));
        let direct_fact_count = facts
            .iter()
            .filter(|fact| {
                self.required_facts
                    .iter()
                    .any(|required| required.matches_fact_id(fact.id))
            })
            .count();
        let support_score = noisy_or(self.supporting_facts, facts);
        let contradiction_score = noisy_or(self.contradicting_facts, facts);
        let phase_compatible = self.phase_allowed.is_empty() || self.phase_allowed.contains(&phase);
        let missing_required_penalty = if required_fact_satisfied {
            0.0
        } else {
            MISSING_REQUIRED_FACT_PENALTY
        };
        let phase_penalty = if phase_compatible {
            0.0
        } else {
            PHASE_MISMATCH_PENALTY
        };
        let evidence_confidence_score = (BASE_CONFIDENCE_SCORE + support_score
            - contradiction_score
            - missing_required_penalty
            - phase_penalty)
            .clamp(0.0, 1.0);
        let impact = self.impact(fact);

        DiagnosisGraphEvaluation {
            required_fact_satisfied,
            support_score,
            contradiction_score,
            phase_compatible,
            direct_fact_count,
            evidence_confidence_score,
            impact,
            impact_scalar: impact.scalar_severity(),
            resolved_severity: self.severity(fact),
            resolved_confidence: self.confidence(fact),
            action_eligibility: self.eligibility,
        }
    }
}

fn noisy_or(weighted_facts: &[WeightedFact], facts: &[GuardianFact]) -> f32 {
    let remainder = weighted_facts
        .iter()
        .flat_map(|weighted_fact| {
            facts
                .iter()
                .filter(move |fact| fact.id == weighted_fact.fact_id)
                .map(move |fact| {
                    1.0 - (weighted_fact.weight * reliability_score(fact.reliability))
                        .clamp(0.0, 1.0)
                })
        })
        .fold(1.0, |product, factor| product * factor);

    (1.0_f32 - remainder).clamp(0.0, 1.0)
}

fn reliability_score(reliability: FactReliability) -> f32 {
    match reliability {
        FactReliability::DirectStructured | FactReliability::ValidatedProbe => 1.0,
        FactReliability::ProcessLifecycle => 0.95,
        FactReliability::ExactClassifier => 0.80,
        FactReliability::HeuristicClassifier => 0.55,
        FactReliability::ExpectedMarkerAbsence => 0.35,
        FactReliability::UserReported => 0.30,
    }
}

const RUNTIME_PHASES: &[OperationPhase] = &[
    OperationPhase::Validating,
    OperationPhase::Preparing,
    OperationPhase::Launching,
    OperationPhase::Repairing,
];
const JVM_PHASES: &[OperationPhase] = &[
    OperationPhase::Validating,
    OperationPhase::Preparing,
    OperationPhase::Launching,
    OperationPhase::Running,
];
const INSTALL_PHASES: &[OperationPhase] = &[
    OperationPhase::Planning,
    OperationPhase::Validating,
    OperationPhase::Installing,
    OperationPhase::Preparing,
];
const DOWNLOAD_PHASES: &[OperationPhase] = &[
    OperationPhase::Validating,
    OperationPhase::Downloading,
    OperationPhase::Installing,
    OperationPhase::Planning,
];
const LAUNCH_PHASES: &[OperationPhase] = &[
    OperationPhase::Preparing,
    OperationPhase::Launching,
    OperationPhase::Running,
];
const PERFORMANCE_PHASES: &[OperationPhase] = &[
    OperationPhase::Planning,
    OperationPhase::Validating,
    OperationPhase::Preparing,
    OperationPhase::Running,
    OperationPhase::Repairing,
];
const PROCESS_PHASES: &[OperationPhase] = &[
    OperationPhase::Launching,
    OperationPhase::Running,
    OperationPhase::Completed,
    OperationPhase::Failed,
];
const STATE_PHASES: &[OperationPhase] = &[
    OperationPhase::Startup,
    OperationPhase::Planning,
    OperationPhase::Validating,
    OperationPhase::Running,
];
const ANY_OWNERSHIP: &[OwnershipClass] = &[];

const PROCESS_CONTRADICTIONS: &[WeightedFact] = &[
    WeightedFact::new(GuardianFactId::BootMarkerObserved, 0.65),
    WeightedFact::new(GuardianFactId::LauncherStopRequested, 0.85),
];
const NO_CONTRADICTIONS: &[WeightedFact] = &[];

const RECORD_ONLY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::None,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const BLOCK_ONLY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const RUNTIME_ATTEMPT_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::RequiredForAttemptAction,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::OneAttemptOverride,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
};
const MANAGED_RUNTIME_REPAIR_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::LauncherManaged,
    journal_requirement: JournalRequirement::RequiredForManagedMutation,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
    destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const RESOURCE_WARNING_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::None,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const EXPLICIT_INTENT_WARNING_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
};
const JVM_ATTEMPT_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::RequiredForAttemptAction,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::OneAttemptOverride,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
};
const MANAGED_ARTIFACT_REPAIR_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::LauncherManaged,
    journal_requirement: JournalRequirement::RequiredForManagedMutation,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
    destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const PROVIDER_RETRY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::Classified,
    journal_requirement: JournalRequirement::RequiredForAttemptAction,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::ProviderRetry,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::None,
};
const USER_OR_UNKNOWN_PROTECTION_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
    user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
};
const PERFORMANCE_RECORD_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::CompositionManaged,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
};
const PERFORMANCE_MEMORY_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::CompositionManaged,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::RepeatedFailureMemory,
    destructive_mutation_risk: DestructiveMutationRisk::None,
    user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
};
const PERFORMANCE_USER_CONFLICT_ELIGIBILITY: ActionEligibility = ActionEligibility {
    ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
    journal_requirement: JournalRequirement::None,
    redaction_requirement: RedactionRequirement::PublicOutcome,
    retry_loop_sensitivity: RetryLoopSensitivity::None,
    destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
    user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
};

const DIAGNOSIS_GRAPH: &[DiagnosisGraphNode] = &[
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("java_override_unavailable"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::JavaOverrideEmpty,
            GuardianFactId::JavaOverrideMissing,
            GuardianFactId::JavaOverrideUndefinedSentinel,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::JavaOverrideEmpty, 1.0),
            WeightedFact::new(GuardianFactId::JavaOverrideMissing, 1.0),
            WeightedFact::new(GuardianFactId::JavaOverrideUndefinedSentinel, 1.0),
        ],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: RUNTIME_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::JavaOverrideUnavailable,
        eligibility: RUNTIME_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Fallback,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("selected_java_runtime_unavailable"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("java_probe_failed"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::JavaProbeFailed])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::JavaProbeFailed, 1.0)],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: RUNTIME_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::High),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: RUNTIME_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Fallback, GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("java_runtime_probe_failed"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("java_runtime_major_mismatch"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::JavaMajorMismatch])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::JavaMajorMismatch, 1.0)],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: RUNTIME_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: RUNTIME_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Fallback, GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("java_runtime_major_mismatch"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("java_runtime_update_too_old"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::JavaUpdateTooOld])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::JavaUpdateTooOld, 1.0)],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: RUNTIME_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: RUNTIME_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Fallback, GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("java_update_too_old"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("managed_runtime_missing"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::ManagedRuntimeMissing,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::ManagedRuntimeMissing,
            0.75,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: RUNTIME_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Recoverable),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::ManagedRuntimeMissing,
        eligibility: RECORD_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::Static("managed_runtime_missing"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("managed_runtime_unavailable_for_platform"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::ManagedRuntimeUnavailableForPlatform,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::ManagedRuntimeUnavailableForPlatform,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("managed_runtime_unavailable_for_platform"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("managed_runtime_rosetta_required"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::ManagedRuntimeRosettaRequired,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::ManagedRuntimeRosettaRequired,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("managed_runtime_rosetta_required"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("managed_runtime_corrupt"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianFactId::ManagedRuntimeCorrupt,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::ManagedRuntimeReadyMarkerMissing, 1.0),
            WeightedFact::new(GuardianFactId::ManagedRuntimeCorrupt, 1.0),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: RUNTIME_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Repairable),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::RepairableCorruption,
        eligibility: MANAGED_RUNTIME_REPAIR_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Repair, GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("managed_runtime_needs_repair"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Readiness,
        domain: DomainTemplate::Static(GuardianDomain::Install),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::VersionJsonMissing,
            GuardianFactId::ParentVersionMissing,
            GuardianFactId::IncompleteInstall,
            GuardianFactId::ClientJarMissing,
            GuardianFactId::LibrariesMissing,
            GuardianFactId::AssetIndexMissing,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::VersionJsonMissing, 1.0),
            WeightedFact::new(GuardianFactId::ParentVersionMissing, 1.0),
            WeightedFact::new(GuardianFactId::IncompleteInstall, 1.0),
            WeightedFact::new(GuardianFactId::ClientJarMissing, 1.0),
            WeightedFact::new(GuardianFactId::LibrariesMissing, 1.0),
            WeightedFact::new(GuardianFactId::AssetIndexMissing, 1.0),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: INSTALL_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::Readiness,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::FactId,
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("launch_command_invalid"),
        domain: DomainTemplate::Static(GuardianDomain::Launch),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::LaunchCommandInvalid,
        ])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::LaunchCommandInvalid, 1.0)],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: LAUNCH_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("launch_command_invalid"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("launch_command_prepared"),
        domain: DomainTemplate::Static(GuardianDomain::Launch),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::LaunchCommandPrepared,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::LaunchCommandPrepared,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: LAUNCH_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Info),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::RecordOnly,
        eligibility: RECORD_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::Static("launch_command_prepared"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::FactId,
        domain: DomainTemplate::FactDomain,
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::LaunchMemoryMinClamped,
            GuardianFactId::LaunchMemoryAllocationLow,
            GuardianFactId::LaunchResourceMemoryPressure,
            GuardianFactId::LaunchResourceCpuPressure,
            GuardianFactId::LaunchResourceInstallPressure,
            GuardianFactId::LaunchResourceDiskPressure,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::LaunchMemoryMinClamped, 0.70),
            WeightedFact::new(GuardianFactId::LaunchMemoryAllocationLow, 0.70),
            WeightedFact::new(GuardianFactId::LaunchResourceMemoryPressure, 0.70),
            WeightedFact::new(GuardianFactId::LaunchResourceCpuPressure, 0.70),
            WeightedFact::new(GuardianFactId::LaunchResourceInstallPressure, 0.70),
            WeightedFact::new(GuardianFactId::LaunchResourceDiskPressure, 0.70),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: LAUNCH_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Warning),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::High),
        impact: ImpactTemplate::ResourcePressure,
        eligibility: RESOURCE_WARNING_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Warn, GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::FactId,
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::FactId,
        domain: DomainTemplate::FactDomain,
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::CustomJavaOverridePresent,
            GuardianFactId::CustomJvmPresetPresent,
            GuardianFactId::CustomJvmArgsPresent,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::CustomJavaOverridePresent, 0.80),
            WeightedFact::new(GuardianFactId::CustomJvmPresetPresent, 0.80),
            WeightedFact::new(GuardianFactId::CustomJvmArgsPresent, 0.80),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: LAUNCH_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Warning),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::High),
        impact: ImpactTemplate::CustomIntent,
        eligibility: EXPLICIT_INTENT_WARNING_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Warn, GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::FactId,
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("jvm_args_empty"),
        domain: DomainTemplate::Static(GuardianDomain::Jvm),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::JvmArgsEmpty])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::JvmArgsEmpty, 1.0)],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: JVM_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Info),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::RecordOnly,
        eligibility: RECORD_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::Static("jvm_args_empty"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("jvm_args_malformed"),
        domain: DomainTemplate::Static(GuardianDomain::Jvm),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::JvmArgsParseFailed])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::JvmArgsParseFailed, 1.0)],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: JVM_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::JvmArgsMalformed,
        eligibility: JVM_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Strip,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("jvm_args_malformed"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("jvm_arg_unsupported"),
        domain: DomainTemplate::Static(GuardianDomain::Jvm),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::JvmArgUnsupportedGc,
            GuardianFactId::JvmArgUnlockOrderInvalid,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::JvmArgUnsupportedGc, 1.0),
            WeightedFact::new(GuardianFactId::JvmArgUnlockOrderInvalid, 1.0),
        ],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: JVM_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: JVM_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Strip,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("jvm_arg_unsupported"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("jvm_arg_unsafe_override"),
        domain: DomainTemplate::Static(GuardianDomain::Jvm),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::JvmArgReservedLauncherFlag,
            GuardianFactId::JvmArgMemoryConflict,
            GuardianFactId::JvmArgUnsafeClasspathOverride,
            GuardianFactId::JvmArgUnsafeNativePathOverride,
            GuardianFactId::JvmArgAgentOverride,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::JvmArgReservedLauncherFlag, 1.0),
            WeightedFact::new(GuardianFactId::JvmArgMemoryConflict, 1.0),
            WeightedFact::new(GuardianFactId::JvmArgUnsafeClasspathOverride, 1.0),
            WeightedFact::new(GuardianFactId::JvmArgUnsafeNativePathOverride, 1.0),
            WeightedFact::new(GuardianFactId::JvmArgAgentOverride, 1.0),
        ],
        contradicting_facts: PROCESS_CONTRADICTIONS,
        phase_allowed: JVM_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::UnsafeJvmOverride,
        eligibility: JVM_ATTEMPT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Strip,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("jvm_arg_unsafe_override"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("launcher_managed_artifact_signature_corrupt"),
        domain: DomainTemplate::FactDomain,
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::LauncherManagedArtifactSignatureCorruption,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::LauncherManagedArtifactSignatureCorruption,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static(
            "launcher_managed_artifact_signature_corrupt",
        ),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("launcher_managed_artifact_corrupt"),
        domain: DomainTemplate::FactDomain,
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::ArtifactChecksumMismatch,
            GuardianFactId::ArtifactSizeMismatch,
            GuardianFactId::ManagedFileCorrupt,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::ArtifactChecksumMismatch, 1.0),
            WeightedFact::new(GuardianFactId::ArtifactSizeMismatch, 1.0),
            WeightedFact::new(GuardianFactId::ManagedFileCorrupt, 1.0),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Repairable),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::RepairableCorruption,
        eligibility: MANAGED_ARTIFACT_REPAIR_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Quarantine,
            GuardianActionKind::Repair,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("managed_artifact_corrupt"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("launcher_managed_artifact_corrupt"),
        domain: DomainTemplate::FactDomain,
        required_facts: &[FactRequirement::Any(&[GuardianFactId::ArtifactMissing])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::ArtifactMissing, 0.80)],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Repairable),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::High),
        impact: ImpactTemplate::RepairableCorruption,
        eligibility: MANAGED_ARTIFACT_REPAIR_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Quarantine,
            GuardianActionKind::Repair,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("managed_artifact_corrupt"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("install_artifact_metadata_invalid"),
        domain: DomainTemplate::Static(GuardianDomain::Install),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::ProviderDataInvalid])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::ProviderDataInvalid, 1.0)],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: INSTALL_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("install_artifact_metadata_invalid"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("install_dependency_failed"),
        domain: DomainTemplate::Static(GuardianDomain::Install),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::InstallDependencyFailed,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::InstallDependencyFailed,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("install_dependency_failed"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("download_unavailable"),
        domain: DomainTemplate::Static(GuardianDomain::Download),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::DownloadProviderUnavailable,
            GuardianFactId::DownloadInterrupted,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::DownloadProviderUnavailable, 0.80),
            WeightedFact::new(GuardianFactId::DownloadInterrupted, 0.80),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Medium),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: PROVIDER_RETRY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::Retry,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("download_unavailable"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("filesystem_permission_denied"),
        domain: DomainTemplate::Static(GuardianDomain::Filesystem),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::FilesystemPermissionDenied,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::FilesystemPermissionDenied,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: INSTALL_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("filesystem_permission_denied"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("temp_file_leftover"),
        domain: DomainTemplate::Static(GuardianDomain::Filesystem),
        required_facts: &[FactRequirement::Any(&[GuardianFactId::TempFileLeftover])],
        supporting_facts: &[WeightedFact::new(GuardianFactId::TempFileLeftover, 1.0)],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("temp_file_leftover"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("atomic_promotion_failed"),
        domain: DomainTemplate::Static(GuardianDomain::Filesystem),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::AtomicPromotionFailed,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::AtomicPromotionFailed,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: DOWNLOAD_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::LaunchBlocking,
        eligibility: BLOCK_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("atomic_promotion_failed"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("artifact_ownership_unsafe"),
        domain: DomainTemplate::Static(GuardianDomain::Filesystem),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::OwnershipUnknown,
            GuardianFactId::PrimitiveRefused,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::OwnershipUnknown, 1.0),
            WeightedFact::new(GuardianFactId::PrimitiveRefused, 1.0),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: INSTALL_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::ArtifactOwnershipUnsafe,
        eligibility: USER_OR_UNKNOWN_PROTECTION_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Block],
        public_reason_template: ReasonTemplate::Static("artifact_ownership_unsafe"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("performance_rules_invalid"),
        domain: DomainTemplate::Static(GuardianDomain::Performance),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::PerformanceRulesInvalid,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::PerformanceRulesInvalid,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: PERFORMANCE_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Degraded),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::High),
        impact: ImpactTemplate::PerformanceRulesInvalid,
        eligibility: PERFORMANCE_RECORD_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
        public_reason_template: ReasonTemplate::Static("performance_rules_invalid"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::FactId,
        domain: DomainTemplate::Static(GuardianDomain::Performance),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::PerformanceHealthDegraded,
            GuardianFactId::PerformanceHealthInvalid,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::PerformanceHealthDegraded, 0.85),
            WeightedFact::new(GuardianFactId::PerformanceHealthInvalid, 1.0),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: PERFORMANCE_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Degraded),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::High),
        impact: ImpactTemplate::PerformanceHealth,
        eligibility: PERFORMANCE_RECORD_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
        public_reason_template: ReasonTemplate::FactId,
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("performance_fallback_selected"),
        domain: DomainTemplate::Static(GuardianDomain::Performance),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::PerformanceFallbackSelected,
            GuardianFactId::PerformanceHealthFallback,
        ])],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::PerformanceFallbackSelected, 0.80),
            WeightedFact::new(GuardianFactId::PerformanceHealthFallback, 0.80),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: PERFORMANCE_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Warning),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::High),
        impact: ImpactTemplate::PerformanceFallback,
        eligibility: PERFORMANCE_RECORD_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
        public_reason_template: ReasonTemplate::Static("performance_fallback_selected"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("performance_repeated_failure_memory"),
        domain: DomainTemplate::Static(GuardianDomain::Performance),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::PerformanceRepeatedFailureMemory,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::PerformanceRepeatedFailureMemory,
            0.95,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: PERFORMANCE_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Degraded),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::High),
        impact: ImpactTemplate::PerformanceRepeatedFailure,
        eligibility: PERFORMANCE_MEMORY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
        public_reason_template: ReasonTemplate::Static("performance_repeated_failure_memory"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("performance_user_owned_conflict"),
        domain: DomainTemplate::Static(GuardianDomain::Performance),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::PerformanceUserOwnedConflict,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::PerformanceUserOwnedConflict,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: PERFORMANCE_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::FactOr(GuardianSeverity::Blocking),
        confidence: ConfidenceTemplate::FactOr(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::PerformanceUserOwnedConflict,
        eligibility: PERFORMANCE_USER_CONFLICT_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[
            GuardianActionKind::RecordOnly,
            GuardianActionKind::Warn,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: ReasonTemplate::Static("performance_user_owned_conflict"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("process_lifecycle_observed"),
        domain: DomainTemplate::Static(GuardianDomain::Session),
        required_facts: &[FactRequirement::ProcessLifecycle],
        supporting_facts: &[
            WeightedFact::new(GuardianFactId::ProcessSpawned, 0.95),
            WeightedFact::new(GuardianFactId::LauncherStopRequested, 0.95),
            WeightedFact::new(GuardianFactId::WatchdogKilledProcess, 0.95),
            WeightedFact::new(GuardianFactId::ExitCodeZero, 0.95),
            WeightedFact::new(GuardianFactId::ExitCodeNonzero, 0.95),
            WeightedFact::new(GuardianFactId::ExitCodeUnknown, 0.95),
            WeightedFact::new(GuardianFactId::BootMarkerObserved, 0.95),
            WeightedFact::new(GuardianFactId::ProcessExited, 0.95),
            WeightedFact::new(GuardianFactId::ProcessExitedBeforeBoot, 0.95),
            WeightedFact::new(GuardianFactId::ProcessExitedAfterBoot, 0.95),
        ],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: PROCESS_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Info),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::High),
        impact: ImpactTemplate::RecordOnly,
        eligibility: RECORD_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::Static("process_lifecycle_observed"),
    },
    DiagnosisGraphNode {
        id: DiagnosisIdTemplate::Static("persisted_state_schema_invalid"),
        domain: DomainTemplate::Static(GuardianDomain::State),
        required_facts: &[FactRequirement::Any(&[
            GuardianFactId::PersistedStateSchemaInvalid,
        ])],
        supporting_facts: &[WeightedFact::new(
            GuardianFactId::PersistedStateSchemaInvalid,
            1.0,
        )],
        contradicting_facts: NO_CONTRADICTIONS,
        phase_allowed: STATE_PHASES,
        ownership_allowed: ANY_OWNERSHIP,
        severity: SeverityTemplate::Static(GuardianSeverity::Warning),
        confidence: ConfidenceTemplate::Static(GuardianConfidence::Confirmed),
        impact: ImpactTemplate::PersistedStateSchemaInvalid,
        eligibility: RECORD_ONLY_ELIGIBILITY,
        target_strategy: AffectedTargetStrategy::FactTargetOrGuardianFallback,
        candidate_actions: &[GuardianActionKind::Warn, GuardianActionKind::RecordOnly],
        public_reason_template: ReasonTemplate::Static("persisted_state_schema_invalid"),
    },
];

#[cfg(test)]
pub(super) fn diagnosis_graph_nodes() -> &'static [DiagnosisGraphNode] {
    DIAGNOSIS_GRAPH
}

pub(super) fn diagnosis_node_for_fact(fact: &GuardianFact) -> Option<&'static DiagnosisGraphNode> {
    DIAGNOSIS_GRAPH.iter().find(|node| node.matches_fact(fact))
}

pub(super) fn graph_policy_input_for_diagnosis(
    diagnosis: &Diagnosis,
) -> Option<DiagnosisGraphPolicyInput> {
    diagnosis.fact_ids.iter().find_map(|fact_id| {
        let fact = synthetic_fact_for_diagnosis(diagnosis, *fact_id);
        DIAGNOSIS_GRAPH
            .iter()
            .find(|node| {
                node.matches_fact(&fact)
                    && node.diagnosis_id(&fact) == diagnosis.id.as_str()
                    && node.domain(&fact) == diagnosis.domain
            })
            .map(|node| DiagnosisGraphPolicyInput {
                resolved_severity: diagnosis.severity,
                resolved_confidence: diagnosis.confidence,
                impact_scalar: diagnosis.impact.scalar_severity(),
                action_eligibility: node.eligibility,
            })
    })
}

fn synthetic_fact_for_diagnosis(diagnosis: &Diagnosis, fact_id: GuardianFactId) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id: fact_id,
        domain: diagnosis.domain,
        phase: diagnosis.phase,
        reliability: FactReliability::DirectStructured,
        severity: Some(diagnosis.severity),
        confidence: Some(diagnosis.confidence),
        ownership: diagnosis.ownership,
        target: diagnosis.affected_targets.first().cloned(),
        fields: Vec::new(),
    }
}

const READINESS_DIAGNOSIS_IDS: &[(GuardianFactId, &str)] = &[
    (
        GuardianFactId::VersionJsonMissing,
        "installed_version_metadata_missing",
    ),
    (
        GuardianFactId::ParentVersionMissing,
        "parent_version_metadata_missing",
    ),
    (GuardianFactId::IncompleteInstall, "install_incomplete"),
    (GuardianFactId::ClientJarMissing, "client_jar_missing"),
    (GuardianFactId::LibrariesMissing, "libraries_missing"),
    (GuardianFactId::AssetIndexMissing, "asset_index_missing"),
];

fn readiness_diagnosis_id(fact_id: GuardianFactId) -> Option<&'static str> {
    READINESS_DIAGNOSIS_IDS
        .iter()
        .find_map(|(candidate, diagnosis)| (*candidate == fact_id).then_some(*diagnosis))
}

fn readiness_state_corruption_impact(fact_id: GuardianFactId) -> f32 {
    if fact_id == GuardianFactId::IncompleteInstall {
        0.75
    } else if [
        GuardianFactId::VersionJsonMissing,
        GuardianFactId::ParentVersionMissing,
    ]
    .contains(&fact_id)
    {
        0.65
    } else if [
        GuardianFactId::ClientJarMissing,
        GuardianFactId::LibrariesMissing,
        GuardianFactId::AssetIndexMissing,
    ]
    .contains(&fact_id)
    {
        0.55
    } else {
        0.50
    }
}

const PROCESS_FACT_IDS: &[GuardianFactId] = &[
    GuardianFactId::ProcessSpawned,
    GuardianFactId::LauncherStopRequested,
    GuardianFactId::WatchdogKilledProcess,
    GuardianFactId::ExitCodeZero,
    GuardianFactId::ExitCodeNonzero,
    GuardianFactId::ExitCodeUnknown,
    GuardianFactId::BootMarkerObserved,
    GuardianFactId::ProcessExited,
    GuardianFactId::ProcessExitedBeforeBoot,
    GuardianFactId::ProcessExitedAfterBoot,
];

fn is_process_fact(id: GuardianFactId) -> bool {
    PROCESS_FACT_IDS.contains(&id)
}
