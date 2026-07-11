use super::{
    FactReliability, GuardianActionKind, GuardianConfidence, GuardianDecision, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext, GuardianSeverity,
    build_safety_case, decide_guardian_policy,
};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, RollbackState, StabilizationSystem,
    TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::GuardianFailureMemoryEntry;
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use axial_performance::{
    BundleHealth, CompositionPlan, PerformanceRulesStatus, RuleSource, RulesCacheState,
    RulesValidation, StateError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianPerformanceOperationKind {
    ApplyManagedComposition,
    RemoveManagedComposition,
    RollbackManagedComposition,
}

#[derive(Clone, Debug)]
pub struct GuardianPerformanceSupervisionRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub phase: OperationPhase,
    pub operation: GuardianPerformanceOperationKind,
    pub target: TargetDescriptor,
    pub facts: &'a [GuardianFact],
    pub fallback_chain_len: usize,
    pub rollback_state: RollbackState,
    pub context: GuardianPolicyContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianPerformanceSupervisionPlan {
    pub operation: GuardianPerformanceOperationKind,
    pub target: TargetDescriptor,
    pub decision: GuardianDecision,
    pub fact_ids: Vec<GuardianFactId>,
    pub fallback_authorized: bool,
    pub rollback_authorized: bool,
    pub max_fallback_attempts: usize,
    pub public_summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianPerformanceSupervisionRejection {
    UnsafeOwnership,
    MissingJournal,
    UnsafePublicBoundary,
    GuardianBlocked,
    FallbackUnavailable,
    RollbackUnavailable,
}

pub fn plan_performance_supervision(
    request: GuardianPerformanceSupervisionRequest<'_>,
) -> Result<GuardianPerformanceSupervisionPlan, GuardianPerformanceSupervisionRejection> {
    if !request.context.public_redaction_ready {
        return Err(GuardianPerformanceSupervisionRejection::UnsafePublicBoundary);
    }
    if !request.context.journal_available {
        return Err(GuardianPerformanceSupervisionRejection::MissingJournal);
    }
    if request.target.ownership != OwnershipClass::CompositionManaged {
        return Err(GuardianPerformanceSupervisionRejection::UnsafeOwnership);
    }
    let decision = if request.facts.is_empty() {
        GuardianDecision {
            operation_id: request.operation_id.clone(),
            mode: request.mode,
            kind: GuardianActionKind::Allow,
            diagnoses: Vec::new(),
            action_plan: None,
        }
    } else {
        let safety_case = build_safety_case(
            request.operation_id.clone(),
            request.mode,
            request.phase,
            request.facts,
        );
        decide_guardian_policy(&safety_case, request.context)
    };

    if !performance_supervision_allows(request.operation, decision.kind) {
        return Err(GuardianPerformanceSupervisionRejection::GuardianBlocked);
    }
    if decision.kind == GuardianActionKind::Fallback && request.fallback_chain_len == 0 {
        return Err(GuardianPerformanceSupervisionRejection::FallbackUnavailable);
    }

    Ok(GuardianPerformanceSupervisionPlan {
        operation: request.operation,
        target: request.target,
        decision,
        fact_ids: request.facts.iter().map(|fact| fact.id).collect(),
        fallback_authorized: matches!(
            request.operation,
            GuardianPerformanceOperationKind::ApplyManagedComposition
        ) && request.fallback_chain_len > 0,
        rollback_authorized: matches!(
            request.operation,
            GuardianPerformanceOperationKind::RollbackManagedComposition
        ) && request.rollback_state == RollbackState::Available,
        max_fallback_attempts: request.fallback_chain_len,
        public_summary: performance_supervision_summary(request.operation),
    })
}

pub fn performance_rules_guardian_facts(
    status: &PerformanceRulesStatus,
    phase: OperationPhase,
) -> Vec<GuardianFact> {
    let invalid_rules = status.validation == RulesValidation::Invalid
        || status.rules_cache.state == RulesCacheState::Invalid;
    if !invalid_rules {
        return Vec::new();
    }

    vec![performance_fact(
        GuardianFactId::PerformanceRulesInvalid,
        phase,
        GuardianSeverity::Degraded,
        if status.validation == RulesValidation::Invalid {
            GuardianConfidence::Confirmed
        } else {
            GuardianConfidence::High
        },
        rules_target(status),
        vec![
            token_field("rule_source", format!("{:?}", status.rule_source)),
            token_field("rule_channel", format!("{:?}", status.rule_channel)),
            token_field("rules_cache", format!("{:?}", status.rules_cache.state)),
        ],
    )]
}

pub fn performance_plan_guardian_facts(
    plan: &CompositionPlan,
    phase: OperationPhase,
) -> Vec<GuardianFact> {
    if plan.fallback_reason.trim().is_empty() {
        return Vec::new();
    }

    vec![performance_fact(
        GuardianFactId::PerformanceFallbackSelected,
        phase,
        GuardianSeverity::Warning,
        GuardianConfidence::High,
        composition_target(&plan.composition_id),
        vec![
            token_field("composition_id", &plan.composition_id),
            token_field("tier", format!("{:?}", plan.tier)),
            token_field(
                "fallback_chain_count",
                plan.fallback_chain.len().to_string(),
            ),
        ],
    )]
}

pub fn performance_health_guardian_facts(
    health: BundleHealth,
    composition_id: &str,
    warnings: &[String],
    phase: OperationPhase,
) -> Vec<GuardianFact> {
    let (id, severity, confidence) = match health {
        BundleHealth::Healthy | BundleHealth::Disabled => return Vec::new(),
        BundleHealth::Degraded => (
            GuardianFactId::PerformanceHealthDegraded,
            GuardianSeverity::Degraded,
            GuardianConfidence::High,
        ),
        BundleHealth::Fallback => (
            GuardianFactId::PerformanceHealthFallback,
            GuardianSeverity::Warning,
            GuardianConfidence::High,
        ),
        BundleHealth::Invalid => (
            GuardianFactId::PerformanceHealthInvalid,
            GuardianSeverity::Blocking,
            GuardianConfidence::Confirmed,
        ),
    };

    vec![performance_fact(
        id,
        phase,
        severity,
        confidence,
        composition_target(composition_id),
        vec![
            token_field("health", format!("{health:?}")),
            token_field("warning_count", warnings.len().to_string()),
        ],
    )]
}

pub fn performance_state_error_guardian_fact(
    error: &StateError,
    phase: OperationPhase,
) -> Option<GuardianFact> {
    let StateError::InvalidOwnership {
        ownership_class, ..
    } = error
    else {
        return None;
    };
    let ownership = if ownership_class.trim().to_ascii_lowercase().contains("user") {
        OwnershipClass::UserOwned
    } else {
        OwnershipClass::Unknown
    };

    Some(performance_fact(
        GuardianFactId::PerformanceUserOwnedConflict,
        phase,
        GuardianSeverity::Blocking,
        GuardianConfidence::Confirmed,
        TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::Artifact,
            "performance_artifact_ownership_conflict",
            ownership,
        ),
        vec![token_field("ownership_class", ownership_class)],
    ))
}

pub fn performance_failure_memory_guardian_fact(
    entry: &GuardianFailureMemoryEntry,
    phase: OperationPhase,
) -> Option<GuardianFact> {
    if entry.domain != GuardianDomain::Performance || entry.occurrence_count < 2 {
        return None;
    }

    Some(performance_fact(
        GuardianFactId::PerformanceRepeatedFailureMemory,
        phase,
        GuardianSeverity::Degraded,
        GuardianConfidence::High,
        entry.target.clone(),
        vec![
            token_field("diagnosis_id", entry.diagnosis_id.as_str()),
            token_field("occurrence_count", entry.occurrence_count.to_string()),
            token_field(
                "repair_attempt_count",
                entry.repair_attempt_count.to_string(),
            ),
        ],
    ))
}

fn performance_fact(
    id: GuardianFactId,
    phase: OperationPhase,
    severity: GuardianSeverity,
    confidence: GuardianConfidence,
    target: TargetDescriptor,
    fields: Vec<EvidenceField>,
) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id,
        domain: GuardianDomain::Performance,
        phase,
        reliability: FactReliability::DirectStructured,
        severity: Some(severity),
        confidence: Some(confidence),
        ownership: target.ownership,
        target: Some(target),
        fields,
    }
}

fn rules_target(status: &PerformanceRulesStatus) -> TargetDescriptor {
    if status.rule_source == RuleSource::Remote || status.remote_refresh {
        classify_current_artifact(
            CurrentArtifact::ExternalPerformanceRules,
            "performance_rules_remote_source",
        )
        .target
    } else {
        classify_current_artifact(
            CurrentArtifact::PerformanceRulesCache,
            "performance_rules_cache",
        )
        .target
    }
}

fn composition_target(composition_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::PerformanceComposition,
        composition_id.trim(),
        OwnershipClass::CompositionManaged,
    )
}

fn performance_supervision_allows(
    operation: GuardianPerformanceOperationKind,
    decision: GuardianActionKind,
) -> bool {
    match operation {
        GuardianPerformanceOperationKind::ApplyManagedComposition => matches!(
            decision,
            GuardianActionKind::Allow
                | GuardianActionKind::RecordOnly
                | GuardianActionKind::Warn
                | GuardianActionKind::Fallback
        ),
        GuardianPerformanceOperationKind::RemoveManagedComposition => matches!(
            decision,
            GuardianActionKind::Allow | GuardianActionKind::RecordOnly | GuardianActionKind::Warn
        ),
        GuardianPerformanceOperationKind::RollbackManagedComposition => matches!(
            decision,
            GuardianActionKind::Allow | GuardianActionKind::RecordOnly | GuardianActionKind::Warn
        ),
    }
}

fn performance_supervision_summary(operation: GuardianPerformanceOperationKind) -> String {
    match operation {
        GuardianPerformanceOperationKind::ApplyManagedComposition => {
            "guardian_supervised_performance_apply".to_string()
        }
        GuardianPerformanceOperationKind::RemoveManagedComposition => {
            "guardian_supervised_performance_remove".to_string()
        }
        GuardianPerformanceOperationKind::RollbackManagedComposition => {
            "guardian_supervised_performance_rollback".to_string()
        }
    }
}

fn token_field(key: impl Into<String>, value: impl AsRef<str>) -> EvidenceField {
    let sanitized = sanitize_evidence_token(value.as_ref(), RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "redacted".to_string());
    EvidenceField::new(key, sanitized, EvidenceSensitivity::Public)
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianPerformanceOperationKind, GuardianPerformanceSupervisionRejection,
        GuardianPerformanceSupervisionRequest, performance_failure_memory_guardian_fact,
        performance_health_guardian_facts, performance_plan_guardian_facts,
        performance_rules_guardian_facts, performance_state_error_guardian_fact,
        plan_performance_supervision,
    };
    use crate::guardian::{
        GuardianActionKind, GuardianConfidence, GuardianDomain, GuardianFactId, GuardianMode,
        GuardianPolicyContext, GuardianSeverity, diagnose_facts,
    };
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, RollbackState, StabilizationSystem, TargetDescriptor,
        TargetKind,
    };
    use crate::state::failure_memory::GuardianFailureMemoryEntry;
    use axial_performance::types::VersionFamily;
    use axial_performance::{
        BundleHealth, CompositionPlan, CompositionTier, PerformanceMode, RuleChannel, RuleSource,
        RulesCacheStatus, RulesValidation, StateError, builtin_manifest, rules_status_for,
    };

    #[test]
    fn invalid_rules_status_maps_to_guardian_performance_fact() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let status = rules_status_for(
            &manifest,
            RuleSource::Remote,
            RuleChannel::Remote,
            RulesCacheStatus::unavailable(),
            true,
            None,
            RulesValidation::Invalid,
        );

        let facts = performance_rules_guardian_facts(&status, OperationPhase::Validating);

        assert_eq!(facts.len(), 1);
        let fact = &facts[0];
        assert_eq!(fact.id.as_str(), "performance_rules_invalid");
        assert_eq!(fact.domain, GuardianDomain::Performance);
        assert_eq!(fact.phase, OperationPhase::Validating);
        assert_eq!(fact.severity, Some(GuardianSeverity::Degraded));
        assert_eq!(fact.confidence, Some(GuardianConfidence::Confirmed));
        assert_eq!(fact.ownership, OwnershipClass::ExternalProviderDerived);
    }

    #[test]
    fn degraded_fallback_and_invalid_health_map_to_distinct_facts() {
        let cases = [
            (
                BundleHealth::Degraded,
                "performance_health_degraded",
                GuardianSeverity::Degraded,
            ),
            (
                BundleHealth::Fallback,
                "performance_health_fallback",
                GuardianSeverity::Warning,
            ),
            (
                BundleHealth::Invalid,
                "performance_health_invalid",
                GuardianSeverity::Blocking,
            ),
        ];

        for (health, expected_id, expected_severity) in cases {
            let facts = performance_health_guardian_facts(
                health,
                "family-f-fabric-core",
                &["managed composition warning".to_string()],
                OperationPhase::Validating,
            );

            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].id.as_str(), expected_id);
            assert_eq!(facts[0].severity, Some(expected_severity));
            assert_eq!(facts[0].ownership, OwnershipClass::CompositionManaged);
        }
    }

    #[test]
    fn fallback_plan_maps_to_record_only_guardian_diagnosis() {
        let plan = CompositionPlan {
            composition_id: "family-f-fabric-core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: Vec::new(),
            jvm_preset: String::new(),
            fallback_chain: vec!["family-f-vanilla-enhanced".to_string()],
            warnings: Vec::new(),
            fallback_reason: "A faster performance bundle is not compatible.".to_string(),
        };

        let facts = performance_plan_guardian_facts(&plan, OperationPhase::Planning);
        let diagnoses = diagnose_facts(&facts, OperationPhase::Planning);

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id.as_str(), "performance_fallback_selected");
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "performance_fallback_selected");
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Warning);
        assert_eq!(diagnoses[0].confidence, GuardianConfidence::High);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::RecordOnly)
        );
        assert!(
            !diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::Fallback)
        );
    }

    #[test]
    fn invalid_ownership_state_error_maps_to_user_owned_conflict_fact() {
        let fact = performance_state_error_guardian_fact(
            &StateError::InvalidOwnership {
                filename: "user.jar".to_string(),
                ownership_class: "user_managed".to_string(),
            },
            OperationPhase::Validating,
        )
        .expect("ownership conflict fact");

        assert_eq!(fact.id.as_str(), "performance_user_owned_conflict");
        assert_eq!(fact.ownership, OwnershipClass::UserOwned);
        assert_eq!(fact.severity, Some(GuardianSeverity::Blocking));
        let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);
        assert_eq!(diagnoses[0].id.as_str(), "performance_user_owned_conflict");
        assert_eq!(diagnoses[0].ownership, OwnershipClass::UserOwned);
    }

    #[test]
    fn repeated_performance_failure_memory_maps_to_guardian_fact() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::PerformanceComposition,
            "family-f-fabric-core",
            OwnershipClass::CompositionManaged,
        );
        let mut entry = GuardianFailureMemoryEntry::observed(
            crate::guardian::DiagnosisId::new("performance_fallback_selected"),
            GuardianDomain::Performance,
            target,
            GuardianMode::Managed,
            Some("intent"),
            "2026-06-15T12:00:00Z",
        );
        entry.occurrence_count = 3;

        let fact = performance_failure_memory_guardian_fact(&entry, OperationPhase::Planning)
            .expect("repeated performance fact");

        assert_eq!(fact.id.as_str(), "performance_repeated_failure_memory");
        assert_eq!(fact.severity, Some(GuardianSeverity::Degraded));
        assert_eq!(fact.confidence, Some(GuardianConfidence::High));
        assert_eq!(fact.ownership, OwnershipClass::CompositionManaged);
    }

    #[test]
    fn performance_supervision_authorizes_managed_fallback_envelope() {
        let plan = CompositionPlan {
            composition_id: "family-f-fabric-core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: Vec::new(),
            jvm_preset: String::new(),
            fallback_chain: vec!["family-f-vanilla-enhanced".to_string()],
            warnings: Vec::new(),
            fallback_reason: "A faster performance bundle is not compatible.".to_string(),
        };
        let facts = performance_plan_guardian_facts(&plan, OperationPhase::Installing);

        let supervision = plan_performance_supervision(GuardianPerformanceSupervisionRequest {
            operation_id: None,
            mode: GuardianMode::Managed,
            phase: OperationPhase::Installing,
            operation: GuardianPerformanceOperationKind::ApplyManagedComposition,
            target: performance_target("family-f-fabric-core", OwnershipClass::CompositionManaged),
            facts: &facts,
            fallback_chain_len: plan.fallback_chain.len(),
            rollback_state: RollbackState::Available,
            context: GuardianPolicyContext::current_operation(),
        })
        .expect("fallback supervision plan");

        assert_eq!(supervision.decision.kind, GuardianActionKind::Warn);
        assert!(supervision.fallback_authorized);
        assert_eq!(supervision.max_fallback_attempts, 1);
        assert_eq!(
            supervision.fact_ids,
            vec![GuardianFactId::PerformanceFallbackSelected]
        );
        assert_eq!(
            supervision.public_summary,
            "guardian_supervised_performance_apply"
        );
    }

    #[test]
    fn performance_supervision_rejects_user_owned_mutation_target() {
        let error = plan_performance_supervision(GuardianPerformanceSupervisionRequest {
            operation_id: None,
            mode: GuardianMode::Managed,
            phase: OperationPhase::Installing,
            operation: GuardianPerformanceOperationKind::ApplyManagedComposition,
            target: performance_target("user-mods", OwnershipClass::UserOwned),
            facts: &[],
            fallback_chain_len: 0,
            rollback_state: RollbackState::Unavailable,
            context: GuardianPolicyContext::current_operation(),
        })
        .expect_err("user-owned performance mutation should reject");

        assert_eq!(
            error,
            GuardianPerformanceSupervisionRejection::UnsafeOwnership
        );
    }

    #[test]
    fn performance_supervision_marks_unavailable_rollback_without_blocking_preflight_error() {
        let supervision = plan_performance_supervision(GuardianPerformanceSupervisionRequest {
            operation_id: None,
            mode: GuardianMode::Managed,
            phase: OperationPhase::RollingBack,
            operation: GuardianPerformanceOperationKind::RollbackManagedComposition,
            target: performance_target("family-f-fabric-core", OwnershipClass::CompositionManaged),
            facts: &[],
            fallback_chain_len: 0,
            rollback_state: RollbackState::Unavailable,
            context: GuardianPolicyContext::current_operation(),
        })
        .expect("rollback supervision plan");

        assert!(!supervision.rollback_authorized);
        assert_eq!(supervision.decision.kind, GuardianActionKind::Allow);
    }

    fn performance_target(id: &str, ownership: OwnershipClass) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::PerformanceComposition,
            id,
            ownership,
        )
    }
}
