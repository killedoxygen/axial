use super::rules::{
    ActionEligibility, DestructiveMutationRisk, JournalRequirement, OwnershipRequirement,
    RedactionRequirement, RetryLoopSensitivity, UserIntentSensitivity, rule_order,
};
use super::{
    ActionPlanPrerequisite, Diagnosis, GuardianAction, GuardianActionKind, GuardianActionPlan,
    GuardianDecision, GuardianMode, GuardianSeverity, SafetyCase,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianPolicyContext {
    pub journal_available: bool,
    pub suppression_active: bool,
    pub public_redaction_ready: bool,
    pub explicit_user_intent: bool,
}

impl GuardianPolicyContext {
    pub fn current_operation() -> Self {
        Self {
            journal_available: true,
            suppression_active: false,
            public_redaction_ready: true,
            explicit_user_intent: false,
        }
    }

    pub fn with_missing_journal(mut self) -> Self {
        self.journal_available = false;
        self
    }

    pub fn with_suppression(mut self) -> Self {
        self.suppression_active = true;
        self
    }

    pub fn with_unredacted_public_boundary(mut self) -> Self {
        self.public_redaction_ready = false;
        self
    }

    pub fn with_explicit_user_intent(mut self) -> Self {
        self.explicit_user_intent = true;
        self
    }
}

impl Default for GuardianPolicyContext {
    fn default() -> Self {
        Self::current_operation()
    }
}

#[derive(Clone, Debug)]
struct SelectedPolicyAction {
    kind: GuardianActionKind,
    prerequisite: ActionPlanPrerequisite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateRejection {
    HardInvariant,
    Mode,
    Suppression,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PolicyReasoningInput {
    action: GuardianActionKind,
    ownership: OwnershipClass,
    ownership_requirement: OwnershipRequirement,
    public_redaction_required: bool,
    journal_required: bool,
    destructive_mutation: bool,
    retry_loop_sensitive: bool,
    user_intent_sensitive: bool,
}

impl PolicyReasoningInput {
    fn public_redaction_blocked(self, context: GuardianPolicyContext) -> bool {
        self.public_redaction_required && !context.public_redaction_ready
    }

    fn journal_blocked(self, context: GuardianPolicyContext) -> bool {
        self.journal_required && !context.journal_available
    }

    fn ownership_blocks_mutation(self) -> bool {
        self.destructive_mutation
            && matches!(
                self.ownership,
                OwnershipClass::UserOwned | OwnershipClass::Unknown
            )
    }

    fn suppression_blocks(self, context: GuardianPolicyContext) -> bool {
        self.retry_loop_sensitive && context.suppression_active
    }

    fn explicit_intent_blocks_automatic_change(self, context: GuardianPolicyContext) -> bool {
        context.explicit_user_intent
            && (self.user_intent_sensitive || action_changes_user_intent(self.action))
    }

    fn hard_invariant_rejects(self, context: GuardianPolicyContext) -> bool {
        self.public_redaction_blocked(context)
            || self.journal_blocked(context)
            || self.ownership_blocks_mutation()
    }
}

pub fn decide_guardian_policy(
    safety_case: &SafetyCase,
    context: GuardianPolicyContext,
) -> GuardianDecision {
    let diagnoses = safety_case
        .diagnoses
        .iter()
        .map(|diagnosis| diagnosis.id())
        .collect::<Vec<_>>();

    if !context.public_redaction_ready {
        return GuardianDecision {
            operation_id: safety_case.operation_id.clone(),
            mode: safety_case.mode,
            kind: GuardianActionKind::Block,
            diagnoses,
            action_plan: None,
        };
    }

    let Some(diagnosis) = strongest_diagnosis(&safety_case.diagnoses) else {
        return GuardianDecision {
            operation_id: safety_case.operation_id.clone(),
            mode: safety_case.mode,
            kind: GuardianActionKind::Allow,
            diagnoses,
            action_plan: None,
        };
    };

    let Some(selection) = select_policy_action(safety_case.mode, diagnosis, context) else {
        return GuardianDecision {
            operation_id: safety_case.operation_id.clone(),
            mode: safety_case.mode,
            kind: GuardianActionKind::Block,
            diagnoses,
            action_plan: None,
        };
    };
    GuardianDecision {
        operation_id: safety_case.operation_id.clone(),
        mode: safety_case.mode,
        kind: selection.kind,
        diagnoses,
        action_plan: Some(GuardianActionPlan::new(
            StabilizationSystem::Guardian,
            selection.prerequisite.clone(),
            vec![GuardianAction {
                kind: selection.kind,
                target: selection.prerequisite.affected_targets.first().cloned(),
                reason: selection.prerequisite.diagnosis_id,
            }],
        )),
    }
}

fn strongest_diagnosis(diagnoses: &[Diagnosis]) -> Option<&Diagnosis> {
    diagnoses.iter().max_by(|left, right| {
        left.priority().cmp(&right.priority()).then_with(|| {
            rule_order(right.id())
                .unwrap_or(usize::MAX)
                .cmp(&rule_order(left.id()).unwrap_or(usize::MAX))
        })
    })
}

fn policy_reasoning_input(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
) -> PolicyReasoningInput {
    let eligibility = diagnosis.eligibility();
    PolicyReasoningInput {
        action,
        ownership: diagnosis.ownership(),
        ownership_requirement: eligibility.ownership_requirement,
        public_redaction_required: action_requires_public_redaction(eligibility),
        journal_required: action_requires_journal(action, eligibility),
        destructive_mutation: action_is_destructive_mutation(action, eligibility),
        retry_loop_sensitive: action_is_retry_loop_sensitive(action, eligibility),
        user_intent_sensitive: action_is_user_intent_sensitive(eligibility),
    }
}

fn select_policy_action(
    mode: GuardianMode,
    diagnosis: &Diagnosis,
    context: GuardianPolicyContext,
) -> Option<SelectedPolicyAction> {
    let prerequisite = public_safe_prerequisite(diagnosis.action_prerequisite());

    if mode == GuardianMode::Disabled {
        return Some(SelectedPolicyAction {
            kind: disabled_mode_action(diagnosis, context),
            prerequisite,
        });
    }

    if context.suppression_active
        && diagnosis
            .candidate_actions()
            .iter()
            .any(|action| policy_reasoning_input(diagnosis, *action).retry_loop_sensitive)
    {
        return Some(SelectedPolicyAction {
            kind: suppression_fallback_action(diagnosis),
            prerequisite,
        });
    }

    if (diagnosis.severity() == GuardianSeverity::Info
        || matches!(diagnosis.id(), super::DiagnosisId::UnknownFailure(_)))
        && diagnosis
            .candidate_actions()
            .contains(&GuardianActionKind::RecordOnly)
    {
        return Some(SelectedPolicyAction {
            kind: GuardianActionKind::RecordOnly,
            prerequisite,
        });
    }

    let mut saw_hard_rejection = false;
    let mut saw_suppression = false;
    let mut candidates = diagnosis.candidate_actions().to_vec();
    candidates.sort_by_key(|action| action_rank(mode, *action));

    for action in candidates {
        match reject_candidate(mode, diagnosis, action, context) {
            None => {
                if diagnosis.ownership() == OwnershipClass::Unknown
                    && action_changes_user_intent(action)
                {
                    continue;
                }
                return Some(SelectedPolicyAction {
                    kind: action,
                    prerequisite,
                });
            }
            Some(CandidateRejection::HardInvariant) => saw_hard_rejection = true,
            Some(CandidateRejection::Suppression) => saw_suppression = true,
            Some(CandidateRejection::Mode) => {}
        }
    }

    let fallback = if saw_hard_rejection || saw_suppression {
        GuardianActionKind::Block
    } else if diagnosis
        .candidate_actions()
        .contains(&GuardianActionKind::AskUser)
    {
        GuardianActionKind::AskUser
    } else if diagnosis
        .candidate_actions()
        .contains(&GuardianActionKind::Warn)
    {
        GuardianActionKind::Warn
    } else if diagnosis
        .candidate_actions()
        .contains(&GuardianActionKind::RecordOnly)
    {
        GuardianActionKind::RecordOnly
    } else {
        GuardianActionKind::Block
    };

    Some(SelectedPolicyAction {
        kind: fallback,
        prerequisite,
    })
}

fn public_safe_prerequisite(prerequisite: ActionPlanPrerequisite) -> ActionPlanPrerequisite {
    let ActionPlanPrerequisite {
        diagnosis_id,
        ownership,
        confidence,
        affected_targets,
        candidate_actions,
    } = prerequisite;
    ActionPlanPrerequisite {
        diagnosis_id,
        ownership,
        confidence,
        affected_targets: affected_targets.iter().map(public_safe_target).collect(),
        candidate_actions,
    }
}

fn public_safe_target(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        target.id.as_str(),
        target.ownership,
    )
}

fn disabled_mode_action(
    diagnosis: &Diagnosis,
    context: GuardianPolicyContext,
) -> GuardianActionKind {
    if !context.public_redaction_ready
        || matches!(
            diagnosis.severity(),
            GuardianSeverity::Blocking | GuardianSeverity::Critical
        )
        || diagnosis
            .candidate_actions()
            .iter()
            .any(|action| hard_invariant_rejects(diagnosis, *action, context))
    {
        GuardianActionKind::Block
    } else {
        GuardianActionKind::RecordOnly
    }
}

fn suppression_fallback_action(diagnosis: &Diagnosis) -> GuardianActionKind {
    if diagnosis
        .candidate_actions()
        .contains(&GuardianActionKind::Block)
    {
        GuardianActionKind::Block
    } else if diagnosis
        .candidate_actions()
        .contains(&GuardianActionKind::AskUser)
    {
        GuardianActionKind::AskUser
    } else if diagnosis
        .candidate_actions()
        .contains(&GuardianActionKind::Warn)
    {
        GuardianActionKind::Warn
    } else {
        GuardianActionKind::RecordOnly
    }
}

fn reject_candidate(
    mode: GuardianMode,
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
) -> Option<CandidateRejection> {
    if hard_invariant_rejects(diagnosis, action, context) {
        return Some(CandidateRejection::HardInvariant);
    }
    if policy_reasoning_input(diagnosis, action).suppression_blocks(context) {
        return Some(CandidateRejection::Suppression);
    }
    if mode_permission(
        mode,
        diagnosis,
        action,
        context,
        policy_reasoning_input(diagnosis, action),
    ) == 0.0
    {
        return Some(CandidateRejection::Mode);
    }
    None
}

fn hard_invariant_rejects(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
) -> bool {
    policy_reasoning_input(diagnosis, action).hard_invariant_rejects(context)
}

fn mode_permission(
    mode: GuardianMode,
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
    reasoning: PolicyReasoningInput,
) -> f32 {
    match mode {
        GuardianMode::Managed => 1.0,
        GuardianMode::Custom => custom_mode_permission(diagnosis, action, context, reasoning),
        GuardianMode::Disabled => {
            if matches!(
                action,
                GuardianActionKind::Allow
                    | GuardianActionKind::RecordOnly
                    | GuardianActionKind::Block
            ) {
                1.0
            } else {
                0.0
            }
        }
    }
}

fn custom_mode_permission(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
    reasoning: PolicyReasoningInput,
) -> f32 {
    match action {
        GuardianActionKind::Allow
        | GuardianActionKind::Warn
        | GuardianActionKind::AskUser
        | GuardianActionKind::Block
        | GuardianActionKind::RecordOnly => 1.0,
        GuardianActionKind::Repair => {
            if !reasoning.explicit_intent_blocks_automatic_change(context)
                && matches!(
                    diagnosis.ownership(),
                    OwnershipClass::LauncherManaged | OwnershipClass::CompositionManaged
                )
            {
                0.85
            } else {
                0.0
            }
        }
        GuardianActionKind::Fallback => {
            if !reasoning.explicit_intent_blocks_automatic_change(context) {
                0.75
            } else {
                0.0
            }
        }
        GuardianActionKind::Retry => {
            if !reasoning.explicit_intent_blocks_automatic_change(context) {
                0.65
            } else {
                0.0
            }
        }
        GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Quarantine => 0.0,
    }
}

fn action_rank(mode: GuardianMode, action: GuardianActionKind) -> u8 {
    if mode == GuardianMode::Disabled {
        return match action {
            GuardianActionKind::RecordOnly => 0,
            GuardianActionKind::Block => 1,
            GuardianActionKind::Allow => 2,
            _ => 100,
        };
    }

    match action {
        GuardianActionKind::Allow => 0,
        GuardianActionKind::Repair => 20,
        GuardianActionKind::Fallback => 25,
        GuardianActionKind::Strip => 35,
        GuardianActionKind::Downgrade => 40,
        GuardianActionKind::Retry => 45,
        GuardianActionKind::Quarantine => 50,
        GuardianActionKind::Warn => 70,
        GuardianActionKind::AskUser => 80,
        GuardianActionKind::RecordOnly => 90,
        GuardianActionKind::Block => 100,
    }
}

fn requires_journal(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Retry
            | GuardianActionKind::Strip
            | GuardianActionKind::Downgrade
            | GuardianActionKind::Fallback
            | GuardianActionKind::Quarantine
    )
}

fn action_requires_public_redaction(eligibility: ActionEligibility) -> bool {
    matches!(
        eligibility.redaction_requirement,
        RedactionRequirement::PublicOutcome
    )
}

fn action_requires_journal(action: GuardianActionKind, eligibility: ActionEligibility) -> bool {
    let eligibility_requires = match eligibility.journal_requirement {
        JournalRequirement::None => false,
        JournalRequirement::RequiredForAttemptAction => is_attempt_action(action),
        JournalRequirement::RequiredForManagedMutation => is_managed_mutation_action(action),
    };
    eligibility_requires || requires_journal(action)
}

fn is_destructive_mutation(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair | GuardianActionKind::Quarantine
    )
}

fn action_is_destructive_mutation(
    action: GuardianActionKind,
    eligibility: ActionEligibility,
) -> bool {
    let eligibility_marks_destructive = match eligibility.destructive_mutation_risk {
        DestructiveMutationRisk::None => false,
        DestructiveMutationRisk::ManagedMutation
        | DestructiveMutationRisk::UserOrUnknownProtected => is_managed_mutation_action(action),
    };
    eligibility_marks_destructive || is_destructive_mutation(action)
}

fn is_loopable_action(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Retry | GuardianActionKind::Repair
    )
}

fn action_is_retry_loop_sensitive(
    action: GuardianActionKind,
    eligibility: ActionEligibility,
) -> bool {
    let eligibility_marks_loop_sensitive = match eligibility.retry_loop_sensitivity {
        RetryLoopSensitivity::None | RetryLoopSensitivity::OneAttemptOverride => false,
        RetryLoopSensitivity::RepairAttempt => action == GuardianActionKind::Repair,
        RetryLoopSensitivity::ProviderRetry => action == GuardianActionKind::Retry,
        RetryLoopSensitivity::RepeatedFailureMemory => is_loopable_action(action),
    };
    eligibility_marks_loop_sensitive || is_loopable_action(action)
}

fn action_is_user_intent_sensitive(eligibility: ActionEligibility) -> bool {
    !matches!(
        eligibility.user_intent_sensitivity,
        UserIntentSensitivity::None
    )
}

fn action_changes_user_intent(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Retry
            | GuardianActionKind::Strip
            | GuardianActionKind::Downgrade
            | GuardianActionKind::Fallback
            | GuardianActionKind::Quarantine
    )
}

fn is_attempt_action(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Retry
            | GuardianActionKind::Strip
            | GuardianActionKind::Downgrade
            | GuardianActionKind::Fallback
    )
}

fn is_managed_mutation_action(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair | GuardianActionKind::Quarantine
    )
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianPolicyContext, decide_guardian_policy, policy_reasoning_input, strongest_diagnosis,
    };
    use crate::guardian::rules::OwnershipRequirement;
    use crate::guardian::{
        Diagnosis, DiagnosisId, FactReliability, GuardianActionKind, GuardianConfidence,
        GuardianDomain, GuardianFact, GuardianFactId, GuardianMode, GuardianSeverity, SafetyCase,
        diagnose,
    };
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{GuardianFailureMemoryEntry, GuardianFailureMemoryStore};

    #[test]
    fn strongest_diagnosis_uses_typed_cross_family_precedence() {
        struct Case {
            facts: Vec<GuardianFact>,
            phase: OperationPhase,
            expected: DiagnosisId,
        }

        let fact = |id, domain, phase, ownership, severity, confidence| GuardianFact {
            operation_id: None,
            id,
            domain,
            phase,
            reliability: FactReliability::DirectStructured,
            severity,
            confidence,
            ownership,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Guardian,
                target_kind_for_domain(domain),
                id.as_str(),
                ownership,
            )),
            fields: Vec::new(),
        };
        let condition = |id, phase| {
            fact(
                id,
                GuardianDomain::Launch,
                phase,
                OwnershipClass::Unknown,
                None,
                None,
            )
        };
        let corruption = |phase| {
            fact(
                GuardianFactId::ManagedRuntimeCorrupt,
                GuardianDomain::Runtime,
                phase,
                OwnershipClass::LauncherManaged,
                None,
                None,
            )
        };

        let cases = [
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::UnknownLaunchFailure,
                        GuardianDomain::Launch,
                        OperationPhase::Preparing,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                    condition(
                        GuardianFactId::LaunchFailureClassified,
                        OperationPhase::Preparing,
                    ),
                    corruption(OperationPhase::Preparing),
                ],
                phase: OperationPhase::Preparing,
                expected: DiagnosisId::ManagedRuntimeCorrupt,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::OutOfMemory,
                        GuardianDomain::Startup,
                        OperationPhase::Launching,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                    condition(
                        GuardianFactId::LaunchFailureClassified,
                        OperationPhase::Launching,
                    ),
                    condition(
                        GuardianFactId::ProcessExitedBeforeBoot,
                        OperationPhase::Launching,
                    ),
                    corruption(OperationPhase::Launching),
                ],
                phase: OperationPhase::Launching,
                expected: DiagnosisId::ManagedRuntimeCorrupt,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::JavaMajorMismatch,
                        GuardianDomain::Runtime,
                        OperationPhase::Preparing,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                    corruption(OperationPhase::Preparing),
                ],
                phase: OperationPhase::Preparing,
                expected: DiagnosisId::JavaRuntimeMajorMismatch,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::OwnershipUnknown,
                        GuardianDomain::Filesystem,
                        OperationPhase::Preparing,
                        OwnershipClass::Unknown,
                        None,
                        None,
                    ),
                    fact(
                        GuardianFactId::JavaMajorMismatch,
                        GuardianDomain::Runtime,
                        OperationPhase::Preparing,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                ],
                phase: OperationPhase::Preparing,
                expected: DiagnosisId::ArtifactOwnershipUnsafe,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::PerformanceRulesInvalid,
                        GuardianDomain::Performance,
                        OperationPhase::Planning,
                        OwnershipClass::CompositionManaged,
                        Some(GuardianSeverity::Degraded),
                        Some(GuardianConfidence::Confirmed),
                    ),
                    fact(
                        GuardianFactId::PerformanceHealthDegraded,
                        GuardianDomain::Performance,
                        OperationPhase::Planning,
                        OwnershipClass::CompositionManaged,
                        Some(GuardianSeverity::Degraded),
                        Some(GuardianConfidence::High),
                    ),
                ],
                phase: OperationPhase::Planning,
                expected: DiagnosisId::PerformanceRulesInvalid,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::PerformanceRepeatedFailureMemory,
                        GuardianDomain::Performance,
                        OperationPhase::Planning,
                        OwnershipClass::CompositionManaged,
                        None,
                        None,
                    ),
                    fact(
                        GuardianFactId::PerformanceHealthDegraded,
                        GuardianDomain::Performance,
                        OperationPhase::Planning,
                        OwnershipClass::CompositionManaged,
                        None,
                        None,
                    ),
                ],
                phase: OperationPhase::Planning,
                expected: DiagnosisId::PerformanceHealthDegraded,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::StartupWindowExpired,
                        GuardianDomain::Startup,
                        OperationPhase::Launching,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                    condition(
                        GuardianFactId::LaunchFailureClassified,
                        OperationPhase::Launching,
                    ),
                    fact(
                        GuardianFactId::PersistedStateSchemaInvalid,
                        GuardianDomain::State,
                        OperationPhase::Launching,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                ],
                phase: OperationPhase::Launching,
                expected: DiagnosisId::PersistedStateSchemaInvalid,
            },
            Case {
                facts: vec![
                    fact(
                        GuardianFactId::StartupWindowExpired,
                        GuardianDomain::Startup,
                        OperationPhase::Launching,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                    condition(
                        GuardianFactId::LaunchFailureClassified,
                        OperationPhase::Launching,
                    ),
                    condition(
                        GuardianFactId::ProcessExitedBeforeBoot,
                        OperationPhase::Launching,
                    ),
                    fact(
                        GuardianFactId::PersistedStateSchemaInvalid,
                        GuardianDomain::State,
                        OperationPhase::Launching,
                        OwnershipClass::LauncherManaged,
                        None,
                        None,
                    ),
                ],
                phase: OperationPhase::Launching,
                expected: DiagnosisId::StartupStalled,
            },
        ];

        for case in cases {
            let diagnoses = diagnose(&case.facts, case.phase);
            assert_eq!(
                strongest_diagnosis(&diagnoses).map(|diagnosis| diagnosis.id()),
                Some(case.expected)
            );
        }
    }

    #[test]
    fn managed_mode_repairs_launcher_managed_corruption() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::LauncherManaged,
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianActionKind::Repair);
        let plan = decision.action_plan.expect("action plan");
        assert_eq!(plan.prerequisite.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(plan.prerequisite.confidence, GuardianConfidence::Confirmed);
        assert_eq!(plan.actions[0].kind, GuardianActionKind::Repair);
    }

    #[test]
    fn policy_action_plan_sanitizes_prerequisite_targets() {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::ManagedRuntimeCorrupt,
            domain: GuardianDomain::Runtime,
            phase: OperationPhase::Preparing,
            reliability: FactReliability::DirectStructured,
            severity: None,
            confidence: None,
            ownership: OwnershipClass::LauncherManaged,
            target: Some(TargetDescriptor {
                system: StabilizationSystem::Guardian,
                kind: TargetKind::Runtime,
                id: r"C:\Users\Alice\java.exe --accessToken secret -Xmx8192M".to_string(),
                ownership: OwnershipClass::LauncherManaged,
            }),
            fields: Vec::new(),
        };
        let diagnosis = diagnose(&[fact], OperationPhase::Preparing)
            .into_iter()
            .next()
            .expect("managed runtime diagnosis");
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
        let plan = decision.action_plan.expect("sanitized plan");
        let encoded = serde_json::to_string(&plan).expect("plan json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(plan.prerequisite.affected_targets[0].id, "target");
        assert_eq!(
            plan.actions[0]
                .target
                .as_ref()
                .map(|target| target.id.as_str()),
            Some("target")
        );
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("accesstoken"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("secret"));
    }

    #[test]
    fn custom_explicit_intent_asks_before_silent_mutation() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::JvmArgUnsupportedGc,
            GuardianDomain::Jvm,
            OperationPhase::Preparing,
            OwnershipClass::UserOwned,
        );
        let safety_case = safety_case(GuardianMode::Custom, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_explicit_user_intent(),
        );

        assert_eq!(decision.kind, GuardianActionKind::AskUser);
    }

    #[test]
    fn disabled_mode_blocks_hard_invariant_even_when_guardian_is_disabled() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::UserOwned,
        );
        let safety_case = safety_case(GuardianMode::Disabled, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianActionKind::Block);
    }

    #[test]
    fn disabled_mode_records_non_blocking_cases_only() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::CustomJavaOverridePresent,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::UserOwned,
        );
        let safety_case = safety_case(GuardianMode::Disabled, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianActionKind::RecordOnly);
    }

    #[test]
    fn unknown_failure_cushioning_records_only_and_remembers_redacted_target() {
        let raw_target = TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Session,
            id: r"C:\Users\Alice\.minecraft\java.exe --accessToken secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::Unknown,
        };
        let ownership = raw_target.ownership;
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::NoStructuredFact(OperationPhase::Launching),
            domain: GuardianDomain::Unknown,
            phase: OperationPhase::Launching,
            reliability: FactReliability::HeuristicClassifier,
            severity: None,
            confidence: None,
            ownership,
            target: Some(TargetDescriptor::new(
                raw_target.system,
                raw_target.kind,
                raw_target.id,
                ownership,
            )),
            fields: Vec::new(),
        };
        let diagnoses = diagnose(&[fact], OperationPhase::Launching);
        let diagnosis = diagnoses
            .first()
            .expect("unknown diagnosis should be generated")
            .clone();

        assert_eq!(diagnosis.id().as_str(), "unknown_failure_launching");
        assert_eq!(diagnosis.domain(), GuardianDomain::Unknown);
        assert_eq!(diagnosis.confidence(), GuardianConfidence::Low);
        assert_eq!(diagnosis.ownership(), OwnershipClass::Unknown);
        for destructive in [GuardianActionKind::Repair, GuardianActionKind::Quarantine] {
            assert!(!diagnosis.candidate_actions().contains(&destructive));
        }

        let safety_case = SafetyCase {
            operation_id: None,
            mode: GuardianMode::Managed,
            phase: OperationPhase::Launching,
            diagnoses: vec![diagnosis.clone()],
        };
        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianActionKind::RecordOnly);
        let plan = decision
            .action_plan
            .as_ref()
            .expect("record-only action plan");
        assert_eq!(plan.actions[0].kind, GuardianActionKind::RecordOnly);
        let safe_target = plan.actions[0].target.clone().expect("record-only target");
        assert_eq!(safe_target.ownership, OwnershipClass::Unknown);
        assert_eq!(safe_target.id, "target");

        let failure_memory = GuardianFailureMemoryStore::new();
        for observed_at in ["2026-06-16T10:00:00Z", "2026-06-16T10:01:00Z"] {
            failure_memory
                .record(GuardianFailureMemoryEntry::observed(
                    diagnosis.id(),
                    diagnosis.domain(),
                    safe_target.clone(),
                    GuardianMode::Managed,
                    None,
                    observed_at,
                ))
                .expect("record unknown failure memory");
        }

        let memory = failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].occurrence_count, 2);
        assert_eq!(memory[0].repair_attempt_count, 0);
        assert_eq!(memory[0].last_action_kind, None);
        assert_eq!(memory[0].last_action_outcome, None);
        assert_eq!(memory[0].ownership, OwnershipClass::Unknown);
        assert!(memory[0].quarantined_target.is_none());

        let encoded = serde_json::to_string(&(decision, memory)).expect("public-safe json");
        let lower = encoded.to_ascii_lowercase();
        assert!(!lower.contains("users"));
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("accesstoken"));
        assert!(!lower.contains("secret"));
        assert!(!lower.contains("-xmx"));
    }

    #[test]
    fn hard_invariant_blocks_unjournaled_mutation() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::LauncherManaged,
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_missing_journal(),
        );

        assert_eq!(decision.kind, GuardianActionKind::Block);
    }

    #[test]
    fn unredacted_public_boundary_blocks_before_action_planning() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::LauncherManaged,
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_unredacted_public_boundary(),
        );

        assert_eq!(decision.kind, GuardianActionKind::Block);
        assert!(decision.action_plan.is_none());
    }

    #[test]
    fn suppression_blocks_repeated_retry_loop() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::DownloadProviderUnavailable,
            GuardianDomain::Download,
            OperationPhase::Downloading,
            OwnershipClass::ExternalProviderDerived,
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_suppression(),
        );

        assert_eq!(decision.kind, GuardianActionKind::Block);
    }

    #[test]
    fn policy_reasoning_consumes_rule_eligibility_inputs() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::JvmArgsParseFailed,
            GuardianDomain::Jvm,
            OperationPhase::Validating,
            OwnershipClass::UserOwned,
        );

        let reasoning = policy_reasoning_input(&diagnosis, GuardianActionKind::Strip);

        assert_eq!(
            reasoning.ownership_requirement,
            OwnershipRequirement::Classified
        );
        assert!(reasoning.public_redaction_required);
        assert!(reasoning.journal_required);
        assert!(!reasoning.destructive_mutation);
        assert!(!reasoning.retry_loop_sensitive);
        assert!(reasoning.user_intent_sensitive);
        assert!(reasoning.public_redaction_blocked(
            GuardianPolicyContext::current_operation().with_unredacted_public_boundary()
        ));
    }

    #[test]
    fn rule_policy_reasoning_truth_table_covers_hard_constraint_inputs() {
        struct Case {
            fact_id: GuardianFactId,
            domain: GuardianDomain,
            phase: OperationPhase,
            ownership: OwnershipClass,
            action: GuardianActionKind,
            ownership_requirement: OwnershipRequirement,
            journal_required: bool,
            destructive_mutation: bool,
            retry_loop_sensitive: bool,
            user_intent_sensitive: bool,
        }

        let cases = [
            Case {
                fact_id: GuardianFactId::ManagedRuntimeReadyMarkerMissing,
                domain: GuardianDomain::Runtime,
                phase: OperationPhase::Preparing,
                ownership: OwnershipClass::LauncherManaged,
                action: GuardianActionKind::Repair,
                ownership_requirement: OwnershipRequirement::LauncherManaged,
                journal_required: true,
                destructive_mutation: true,
                retry_loop_sensitive: true,
                user_intent_sensitive: false,
            },
            Case {
                fact_id: GuardianFactId::ArtifactChecksumMismatch,
                domain: GuardianDomain::Install,
                phase: OperationPhase::Downloading,
                ownership: OwnershipClass::UserOwned,
                action: GuardianActionKind::Repair,
                ownership_requirement: OwnershipRequirement::LauncherManaged,
                journal_required: true,
                destructive_mutation: true,
                retry_loop_sensitive: true,
                user_intent_sensitive: false,
            },
            Case {
                fact_id: GuardianFactId::DownloadProviderUnavailable,
                domain: GuardianDomain::Download,
                phase: OperationPhase::Downloading,
                ownership: OwnershipClass::ExternalProviderDerived,
                action: GuardianActionKind::Retry,
                ownership_requirement: OwnershipRequirement::Classified,
                journal_required: true,
                destructive_mutation: false,
                retry_loop_sensitive: true,
                user_intent_sensitive: false,
            },
            Case {
                fact_id: GuardianFactId::PerformanceUserOwnedConflict,
                domain: GuardianDomain::Performance,
                phase: OperationPhase::Planning,
                ownership: OwnershipClass::UserOwned,
                action: GuardianActionKind::AskUser,
                ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
                journal_required: false,
                destructive_mutation: false,
                retry_loop_sensitive: false,
                user_intent_sensitive: true,
            },
            Case {
                fact_id: GuardianFactId::ExitCodeZero,
                domain: GuardianDomain::Session,
                phase: OperationPhase::Running,
                ownership: OwnershipClass::LauncherManaged,
                action: GuardianActionKind::RecordOnly,
                ownership_requirement: OwnershipRequirement::None,
                journal_required: false,
                destructive_mutation: false,
                retry_loop_sensitive: false,
                user_intent_sensitive: false,
            },
        ];

        for case in cases {
            let diagnosis = rule_diagnosis(case.fact_id, case.domain, case.phase, case.ownership);
            let reasoning = policy_reasoning_input(&diagnosis, case.action);

            assert_eq!(
                reasoning.ownership_requirement,
                case.ownership_requirement,
                "{}",
                case.fact_id.as_str()
            );
            assert_eq!(
                reasoning.journal_required,
                case.journal_required,
                "{}",
                case.fact_id.as_str()
            );
            assert_eq!(
                reasoning.destructive_mutation,
                case.destructive_mutation,
                "{}",
                case.fact_id.as_str()
            );
            assert_eq!(
                reasoning.retry_loop_sensitive,
                case.retry_loop_sensitive,
                "{}",
                case.fact_id.as_str()
            );
            assert_eq!(
                reasoning.user_intent_sensitive,
                case.user_intent_sensitive,
                "{}",
                case.fact_id.as_str()
            );
            assert_eq!(
                reasoning.journal_blocked(
                    GuardianPolicyContext::current_operation().with_missing_journal()
                ),
                case.journal_required,
                "{}",
                case.fact_id.as_str()
            );
        }
    }

    fn safety_case(mode: GuardianMode, diagnosis: Diagnosis) -> SafetyCase {
        SafetyCase {
            operation_id: None,
            mode,
            phase: OperationPhase::Preparing,
            diagnoses: vec![diagnosis],
        }
    }

    fn rule_diagnosis(
        fact_id: GuardianFactId,
        domain: GuardianDomain,
        phase: OperationPhase,
        ownership: OwnershipClass,
    ) -> Diagnosis {
        let fact = GuardianFact {
            operation_id: None,
            id: fact_id,
            domain,
            phase,
            reliability: FactReliability::DirectStructured,
            severity: None,
            confidence: None,
            ownership,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Guardian,
                target_kind_for_domain(domain),
                fact_id.as_str(),
                ownership,
            )),
            fields: Vec::new(),
        };
        let diagnoses = diagnose(&[fact], phase);
        assert_eq!(diagnoses.len(), 1, "{}", fact_id.as_str());
        diagnoses
            .into_iter()
            .next()
            .expect("rule diagnosis should exist")
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
}
