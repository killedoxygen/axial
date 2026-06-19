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
    Any(&'static [&'static str]),
    ProcessLifecycle,
}

impl FactRequirement {
    pub(super) fn matches_fact_id(self, fact_id: &str) -> bool {
        match self {
            Self::Any(fact_ids) => fact_ids.contains(&fact_id),
            Self::ProcessLifecycle => is_process_fact(fact_id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WeightedFact {
    pub(super) fact_id: &'static str,
    pub(super) weight: f32,
}

impl WeightedFact {
    const fn new(fact_id: &'static str, weight: f32) -> Self {
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
            .any(|required| required.matches_fact_id(fact.id.as_str()))
    }

    pub(super) fn diagnosis_id(&self, fact: &GuardianFact) -> String {
        match self.id {
            DiagnosisIdTemplate::Static(id) => id.to_string(),
            DiagnosisIdTemplate::FactId => fact.id.as_str().to_string(),
            DiagnosisIdTemplate::Readiness => readiness_diagnosis_id(fact.id.as_str()).to_string(),
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
                state_corruption_impact: readiness_state_corruption_impact(fact.id.as_str()),
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
                state_corruption_impact: if fact.id.as_str() == "performance_health_invalid" {
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
        let required_fact_satisfied = self.required_facts.iter().all(|required| {
            facts
                .iter()
                .any(|fact| required.matches_fact_id(fact.id.as_str()))
        });
        let direct_fact_count = facts
            .iter()
            .filter(|fact| {
                self.required_facts
                    .iter()
                    .any(|required| required.matches_fact_id(fact.id.as_str()))
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
                .filter(move |fact| fact.id.as_str() == weighted_fact.fact_id)
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
    WeightedFact::new("boot_marker_observed", 0.65),
    WeightedFact::new("launcher_stop_requested", 0.85),
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
            "java_override_empty",
            "java_override_missing",
            "java_override_undefined_sentinel",
        ])],
        supporting_facts: &[
            WeightedFact::new("java_override_empty", 1.0),
            WeightedFact::new("java_override_missing", 1.0),
            WeightedFact::new("java_override_undefined_sentinel", 1.0),
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
        required_facts: &[FactRequirement::Any(&["java_probe_failed"])],
        supporting_facts: &[WeightedFact::new("java_probe_failed", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["java_major_mismatch"])],
        supporting_facts: &[WeightedFact::new("java_major_mismatch", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["java_update_too_old"])],
        supporting_facts: &[WeightedFact::new("java_update_too_old", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["managed_runtime_missing"])],
        supporting_facts: &[WeightedFact::new("managed_runtime_missing", 0.75)],
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
        id: DiagnosisIdTemplate::Static("managed_runtime_corrupt"),
        domain: DomainTemplate::Static(GuardianDomain::Runtime),
        required_facts: &[FactRequirement::Any(&[
            "managed_runtime_ready_marker_missing",
            "managed_runtime_corrupt",
        ])],
        supporting_facts: &[
            WeightedFact::new("managed_runtime_ready_marker_missing", 1.0),
            WeightedFact::new("managed_runtime_corrupt", 1.0),
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
            "version_json_missing",
            "parent_version_missing",
            "incomplete_install",
            "client_jar_missing",
            "libraries_missing",
            "asset_index_missing",
        ])],
        supporting_facts: &[
            WeightedFact::new("version_json_missing", 1.0),
            WeightedFact::new("parent_version_missing", 1.0),
            WeightedFact::new("incomplete_install", 1.0),
            WeightedFact::new("client_jar_missing", 1.0),
            WeightedFact::new("libraries_missing", 1.0),
            WeightedFact::new("asset_index_missing", 1.0),
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
        required_facts: &[FactRequirement::Any(&["launch_command_invalid"])],
        supporting_facts: &[WeightedFact::new("launch_command_invalid", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["launch_command_prepared"])],
        supporting_facts: &[WeightedFact::new("launch_command_prepared", 1.0)],
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
            "launch_memory_min_clamped",
            "launch_memory_allocation_low",
            "launch_resource_memory_pressure",
            "launch_resource_cpu_pressure",
            "launch_resource_install_pressure",
            "launch_resource_disk_pressure",
        ])],
        supporting_facts: &[
            WeightedFact::new("launch_memory_min_clamped", 0.70),
            WeightedFact::new("launch_memory_allocation_low", 0.70),
            WeightedFact::new("launch_resource_memory_pressure", 0.70),
            WeightedFact::new("launch_resource_cpu_pressure", 0.70),
            WeightedFact::new("launch_resource_install_pressure", 0.70),
            WeightedFact::new("launch_resource_disk_pressure", 0.70),
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
            "custom_java_override_present",
            "custom_jvm_preset_present",
            "custom_jvm_args_present",
        ])],
        supporting_facts: &[
            WeightedFact::new("custom_java_override_present", 0.80),
            WeightedFact::new("custom_jvm_preset_present", 0.80),
            WeightedFact::new("custom_jvm_args_present", 0.80),
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
        required_facts: &[FactRequirement::Any(&["jvm_args_empty"])],
        supporting_facts: &[WeightedFact::new("jvm_args_empty", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["jvm_args_parse_failed"])],
        supporting_facts: &[WeightedFact::new("jvm_args_parse_failed", 1.0)],
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
            "jvm_arg_unsupported_gc",
            "jvm_arg_unlock_order_invalid",
        ])],
        supporting_facts: &[
            WeightedFact::new("jvm_arg_unsupported_gc", 1.0),
            WeightedFact::new("jvm_arg_unlock_order_invalid", 1.0),
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
            "jvm_arg_reserved_launcher_flag",
            "jvm_arg_memory_conflict",
            "jvm_arg_unsafe_classpath_override",
            "jvm_arg_unsafe_native_path_override",
            "jvm_arg_agent_override",
        ])],
        supporting_facts: &[
            WeightedFact::new("jvm_arg_reserved_launcher_flag", 1.0),
            WeightedFact::new("jvm_arg_memory_conflict", 1.0),
            WeightedFact::new("jvm_arg_unsafe_classpath_override", 1.0),
            WeightedFact::new("jvm_arg_unsafe_native_path_override", 1.0),
            WeightedFact::new("jvm_arg_agent_override", 1.0),
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
            "launcher_managed_artifact_signature_corruption",
        ])],
        supporting_facts: &[WeightedFact::new(
            "launcher_managed_artifact_signature_corruption",
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
            "artifact_checksum_mismatch",
            "artifact_size_mismatch",
            "managed_file_corrupt",
        ])],
        supporting_facts: &[
            WeightedFact::new("artifact_checksum_mismatch", 1.0),
            WeightedFact::new("artifact_size_mismatch", 1.0),
            WeightedFact::new("managed_file_corrupt", 1.0),
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
        required_facts: &[FactRequirement::Any(&["artifact_missing"])],
        supporting_facts: &[WeightedFact::new("artifact_missing", 0.80)],
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
        required_facts: &[FactRequirement::Any(&["provider_data_invalid"])],
        supporting_facts: &[WeightedFact::new("provider_data_invalid", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["install_dependency_failed"])],
        supporting_facts: &[WeightedFact::new("install_dependency_failed", 1.0)],
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
            "download_provider_unavailable",
            "download_interrupted",
        ])],
        supporting_facts: &[
            WeightedFact::new("download_provider_unavailable", 0.80),
            WeightedFact::new("download_interrupted", 0.80),
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
        required_facts: &[FactRequirement::Any(&["filesystem_permission_denied"])],
        supporting_facts: &[WeightedFact::new("filesystem_permission_denied", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["temp_file_leftover"])],
        supporting_facts: &[WeightedFact::new("temp_file_leftover", 1.0)],
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
        required_facts: &[FactRequirement::Any(&["atomic_promotion_failed"])],
        supporting_facts: &[WeightedFact::new("atomic_promotion_failed", 1.0)],
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
            "ownership_unknown",
            "primitive_refused",
        ])],
        supporting_facts: &[
            WeightedFact::new("ownership_unknown", 1.0),
            WeightedFact::new("primitive_refused", 1.0),
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
        required_facts: &[FactRequirement::Any(&["performance_rules_invalid"])],
        supporting_facts: &[WeightedFact::new("performance_rules_invalid", 1.0)],
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
            "performance_health_degraded",
            "performance_health_invalid",
        ])],
        supporting_facts: &[
            WeightedFact::new("performance_health_degraded", 0.85),
            WeightedFact::new("performance_health_invalid", 1.0),
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
            "performance_fallback_selected",
            "performance_health_fallback",
        ])],
        supporting_facts: &[
            WeightedFact::new("performance_fallback_selected", 0.80),
            WeightedFact::new("performance_health_fallback", 0.80),
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
            "performance_repeated_failure_memory",
        ])],
        supporting_facts: &[WeightedFact::new(
            "performance_repeated_failure_memory",
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
        required_facts: &[FactRequirement::Any(&["performance_user_owned_conflict"])],
        supporting_facts: &[WeightedFact::new("performance_user_owned_conflict", 1.0)],
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
            WeightedFact::new("process_spawned", 0.95),
            WeightedFact::new("launcher_stop_requested", 0.95),
            WeightedFact::new("watchdog_killed_process", 0.95),
            WeightedFact::new("exit_code_zero", 0.95),
            WeightedFact::new("exit_code_nonzero", 0.95),
            WeightedFact::new("exit_code_unknown", 0.95),
            WeightedFact::new("boot_marker_observed", 0.95),
            WeightedFact::new("process_exited", 0.95),
            WeightedFact::new("process_exited_before_boot", 0.95),
            WeightedFact::new("process_exited_after_boot", 0.95),
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
        required_facts: &[FactRequirement::Any(&["persisted_state_schema_invalid"])],
        supporting_facts: &[WeightedFact::new("persisted_state_schema_invalid", 1.0)],
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
        let fact = synthetic_fact_for_diagnosis(diagnosis, fact_id);
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

fn synthetic_fact_for_diagnosis(diagnosis: &Diagnosis, fact_id: &str) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id: GuardianFactId::new(fact_id),
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

fn readiness_diagnosis_id(fact_id: &str) -> &'static str {
    match fact_id {
        "version_json_missing" => "installed_version_metadata_missing",
        "parent_version_missing" => "parent_version_metadata_missing",
        "incomplete_install" => "install_incomplete",
        "client_jar_missing" => "client_jar_missing",
        "libraries_missing" => "libraries_missing",
        "asset_index_missing" => "asset_index_missing",
        "launcher_managed_artifact_signature_corruption" => {
            "launcher_managed_artifact_signature_corrupt"
        }
        _ => "launch_readiness_blocking",
    }
}

fn readiness_state_corruption_impact(fact_id: &str) -> f32 {
    match fact_id {
        "incomplete_install" => 0.75,
        "version_json_missing" | "parent_version_missing" => 0.65,
        "client_jar_missing" | "libraries_missing" | "asset_index_missing" => 0.55,
        "launcher_managed_artifact_signature_corruption" => 0.70,
        _ => 0.50,
    }
}

fn is_process_fact(id: &str) -> bool {
    matches!(
        id,
        "process_spawned"
            | "launcher_stop_requested"
            | "watchdog_killed_process"
            | "exit_code_zero"
            | "exit_code_nonzero"
            | "exit_code_unknown"
            | "boot_marker_observed"
            | "process_exited"
            | "process_exited_before_boot"
            | "process_exited_after_boot"
    )
}
