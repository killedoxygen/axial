use super::inference_graph::{
    ActionEligibility, DestructiveMutationRisk, DiagnosisGraphPolicyInput, JournalRequirement,
    OwnershipRequirement, RedactionRequirement, RetryLoopSensitivity, UserIntentSensitivity,
    graph_policy_input_for_diagnosis,
};
use super::{
    ActionPlanPrerequisite, Diagnosis, GuardianAction, GuardianActionKind, GuardianActionPlan,
    GuardianConfidence, GuardianDecision, GuardianDecisionKind, GuardianMode, GuardianSeverity,
    SafetyCase, SafetyOutcome,
};
use crate::observability::{RedactionAudience, sanitize_evidence_text};
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
    graph: Option<DiagnosisGraphPolicyInput>,
    action: GuardianActionKind,
    ownership: OwnershipClass,
    ownership_requirement: OwnershipRequirement,
    resolved_severity: GuardianSeverity,
    resolved_confidence: GuardianConfidence,
    impact_scalar: f32,
    public_redaction_required: bool,
    journal_required: bool,
    destructive_mutation: bool,
    retry_loop_sensitive: bool,
    user_intent_sensitive: bool,
}

impl PolicyReasoningInput {
    fn pressure_score(self) -> f32 {
        severity_score(self.resolved_severity).max(self.impact_scalar)
            * confidence_score(self.resolved_confidence)
    }

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

    fn memory_penalty(self, context: GuardianPolicyContext) -> f32 {
        if self.suppression_blocks(context) {
            1.00
        } else {
            0.0
        }
    }
}

pub fn decide_guardian_policy(
    safety_case: &SafetyCase,
    context: GuardianPolicyContext,
) -> GuardianDecision {
    let diagnoses = safety_case
        .diagnoses
        .iter()
        .map(|diagnosis| diagnosis.id.clone())
        .collect::<Vec<_>>();

    if !context.public_redaction_ready {
        return GuardianDecision {
            operation_id: safety_case.operation_id.clone(),
            mode: safety_case.mode,
            kind: GuardianDecisionKind::Block,
            diagnoses,
            action_plan: None,
        };
    }

    let Some(diagnosis) = strongest_diagnosis(&safety_case.diagnoses) else {
        return GuardianDecision {
            operation_id: safety_case.operation_id.clone(),
            mode: safety_case.mode,
            kind: GuardianDecisionKind::Allow,
            diagnoses,
            action_plan: None,
        };
    };

    let Some(selection) = select_policy_action(safety_case.mode, diagnosis, context) else {
        return GuardianDecision {
            operation_id: safety_case.operation_id.clone(),
            mode: safety_case.mode,
            kind: GuardianDecisionKind::Block,
            diagnoses,
            action_plan: None,
        };
    };
    let kind = decision_kind_for_action(selection.kind);
    GuardianDecision {
        operation_id: safety_case.operation_id.clone(),
        mode: safety_case.mode,
        kind,
        diagnoses,
        action_plan: Some(GuardianActionPlan::new(
            StabilizationSystem::Guardian,
            selection.prerequisite.clone(),
            vec![GuardianAction {
                kind: selection.kind,
                target: selection.prerequisite.affected_targets.first().cloned(),
                reason: selection.prerequisite.diagnosis_id.clone(),
            }],
        )),
    }
}

pub fn decision_pressure_score(diagnosis: &Diagnosis) -> f32 {
    policy_pressure_input(diagnosis).pressure_score()
}

pub fn action_safety_score(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    mode: GuardianMode,
    context: GuardianPolicyContext,
) -> f32 {
    let reasoning = policy_reasoning_input(diagnosis, action);
    let permission = mode_permission(mode, diagnosis, action, context, reasoning);
    let reversibility = reversibility_score(action);
    let ownership = ownership_risk_for_action(diagnosis.ownership, action);
    let memory = reasoning.memory_penalty(context);
    permission * reversibility * (1.0 - ownership) * (1.0 - memory)
}

pub fn launch_summary_decision_kind(
    summary: &croopor_launcher::GuardianSummary,
) -> GuardianDecisionKind {
    match summary.decision {
        croopor_launcher::GuardianDecision::Allowed => GuardianDecisionKind::Allow,
        croopor_launcher::GuardianDecision::Warned => GuardianDecisionKind::Warn,
        croopor_launcher::GuardianDecision::Blocked => GuardianDecisionKind::Block,
        croopor_launcher::GuardianDecision::Intervened => summary
            .interventions
            .first()
            .map(|intervention| launch_intervention_decision_kind(intervention.kind))
            .unwrap_or(GuardianDecisionKind::RecordOnly),
    }
}

pub fn launch_summary_safety_outcome(summary: &croopor_launcher::GuardianSummary) -> SafetyOutcome {
    let decision = launch_summary_decision_kind(summary);
    SafetyOutcome {
        decision,
        summary: summary
            .message
            .as_deref()
            .and_then(|message| sanitize_public_text(message, 180))
            .unwrap_or_else(|| launch_summary_fallback_message(decision).to_string()),
        detail: summary
            .details
            .iter()
            .find_map(|detail| sanitize_public_text(detail, 240)),
        diagnoses: Vec::new(),
    }
}

fn strongest_diagnosis(diagnoses: &[Diagnosis]) -> Option<&Diagnosis> {
    diagnoses.iter().max_by(|left, right| {
        decision_pressure_score(left)
            .partial_cmp(&decision_pressure_score(right))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn policy_pressure_input(diagnosis: &Diagnosis) -> PolicyReasoningInput {
    policy_reasoning_input(diagnosis, GuardianActionKind::RecordOnly)
}

fn policy_reasoning_input(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
) -> PolicyReasoningInput {
    let graph = graph_policy_input_for_diagnosis(diagnosis);
    let eligibility = graph.map(|input| input.action_eligibility);
    PolicyReasoningInput {
        graph,
        action,
        ownership: diagnosis.ownership,
        ownership_requirement: ownership_requirement(eligibility),
        resolved_severity: graph
            .map(|input| input.resolved_severity)
            .unwrap_or(diagnosis.severity),
        resolved_confidence: graph
            .map(|input| input.resolved_confidence)
            .unwrap_or(diagnosis.confidence),
        impact_scalar: graph
            .map(|input| input.impact_scalar)
            .unwrap_or_else(|| diagnosis.impact.scalar_severity()),
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
    let prerequisite = public_safe_prerequisite(diagnosis.action_prerequisite().ok()?);

    if mode == GuardianMode::Disabled {
        return Some(SelectedPolicyAction {
            kind: disabled_mode_action(diagnosis, context),
            prerequisite,
        });
    }

    if context.suppression_active
        && diagnosis
            .candidate_actions
            .iter()
            .any(|action| policy_reasoning_input(diagnosis, *action).retry_loop_sensitive)
    {
        return Some(SelectedPolicyAction {
            kind: suppression_fallback_action(diagnosis),
            prerequisite,
        });
    }

    if decision_pressure_score(diagnosis) < 0.20
        && diagnosis
            .candidate_actions
            .contains(&GuardianActionKind::RecordOnly)
    {
        return Some(SelectedPolicyAction {
            kind: GuardianActionKind::RecordOnly,
            prerequisite,
        });
    }

    let mut saw_hard_rejection = false;
    let mut saw_suppression = false;
    let mut candidates = diagnosis.candidate_actions.clone();
    candidates.sort_by_key(|action| action_rank(mode, *action));

    for action in candidates {
        match reject_candidate(mode, diagnosis, action, context) {
            None => {
                if action_safety_score(diagnosis, action, mode, context) > 0.0 {
                    return Some(SelectedPolicyAction {
                        kind: action,
                        prerequisite,
                    });
                }
            }
            Some(CandidateRejection::HardInvariant) => saw_hard_rejection = true,
            Some(CandidateRejection::Suppression) => saw_suppression = true,
            Some(CandidateRejection::Mode) => {}
        }
    }

    let fallback = if saw_hard_rejection || saw_suppression {
        GuardianActionKind::Block
    } else if diagnosis
        .candidate_actions
        .contains(&GuardianActionKind::AskUser)
    {
        GuardianActionKind::AskUser
    } else if diagnosis
        .candidate_actions
        .contains(&GuardianActionKind::Warn)
    {
        GuardianActionKind::Warn
    } else if diagnosis
        .candidate_actions
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
        || diagnosis.impact.privacy_risk > 0.0
        || diagnosis.impact.data_loss_risk > 0.0
        || matches!(
            diagnosis.severity,
            GuardianSeverity::Blocking | GuardianSeverity::Critical
        )
        || diagnosis
            .candidate_actions
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
        .candidate_actions
        .contains(&GuardianActionKind::Block)
    {
        GuardianActionKind::Block
    } else if diagnosis
        .candidate_actions
        .contains(&GuardianActionKind::AskUser)
    {
        GuardianActionKind::AskUser
    } else if diagnosis
        .candidate_actions
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
                    diagnosis.ownership,
                    OwnershipClass::LauncherManaged | OwnershipClass::CompositionManaged
                )
            {
                0.85
            } else {
                0.0
            }
        }
        GuardianActionKind::Fallback | GuardianActionKind::Degrade => {
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
        GuardianActionKind::Replace
        | GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Quarantine
        | GuardianActionKind::Rollback => 0.0,
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
        GuardianActionKind::Degrade => 30,
        GuardianActionKind::Strip => 35,
        GuardianActionKind::Downgrade => 40,
        GuardianActionKind::Retry => 45,
        GuardianActionKind::Quarantine => 50,
        GuardianActionKind::Rollback => 55,
        GuardianActionKind::Replace => 60,
        GuardianActionKind::Warn => 70,
        GuardianActionKind::AskUser => 80,
        GuardianActionKind::RecordOnly => 90,
        GuardianActionKind::Block => 100,
    }
}

fn decision_kind_for_action(action: GuardianActionKind) -> GuardianDecisionKind {
    match action {
        GuardianActionKind::Allow => GuardianDecisionKind::Allow,
        GuardianActionKind::Warn => GuardianDecisionKind::Warn,
        GuardianActionKind::Repair => GuardianDecisionKind::Repair,
        GuardianActionKind::Retry => GuardianDecisionKind::Retry,
        GuardianActionKind::Replace => GuardianDecisionKind::Replace,
        GuardianActionKind::Strip => GuardianDecisionKind::Strip,
        GuardianActionKind::Downgrade => GuardianDecisionKind::Downgrade,
        GuardianActionKind::Degrade => GuardianDecisionKind::Degrade,
        GuardianActionKind::Fallback => GuardianDecisionKind::Fallback,
        GuardianActionKind::Quarantine => GuardianDecisionKind::Quarantine,
        GuardianActionKind::Rollback => GuardianDecisionKind::Rollback,
        GuardianActionKind::AskUser => GuardianDecisionKind::AskUser,
        GuardianActionKind::Block => GuardianDecisionKind::Block,
        GuardianActionKind::RecordOnly => GuardianDecisionKind::RecordOnly,
    }
}

fn severity_score(severity: GuardianSeverity) -> f32 {
    match severity {
        GuardianSeverity::Info => 0.10,
        GuardianSeverity::Warning => 0.25,
        GuardianSeverity::Degraded => 0.45,
        GuardianSeverity::Repairable | GuardianSeverity::Recoverable => 0.60,
        GuardianSeverity::Blocking => 0.85,
        GuardianSeverity::Critical => 1.00,
    }
}

fn confidence_score(confidence: GuardianConfidence) -> f32 {
    match confidence {
        GuardianConfidence::Low => 0.25,
        GuardianConfidence::Medium => 0.55,
        GuardianConfidence::High => 0.80,
        GuardianConfidence::Confirmed | GuardianConfidence::Certain => 1.00,
    }
}

fn reversibility_score(action: GuardianActionKind) -> f32 {
    match action {
        GuardianActionKind::RecordOnly => 1.00,
        GuardianActionKind::Allow
        | GuardianActionKind::Warn
        | GuardianActionKind::AskUser
        | GuardianActionKind::Block => 0.95,
        GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Degrade
        | GuardianActionKind::Fallback
        | GuardianActionKind::Retry => 0.85,
        GuardianActionKind::Rollback => 0.75,
        GuardianActionKind::Repair | GuardianActionKind::Replace => 0.65,
        GuardianActionKind::Quarantine => 0.60,
    }
}

fn ownership_risk_for_action(ownership: OwnershipClass, action: GuardianActionKind) -> f32 {
    if matches!(
        action,
        GuardianActionKind::Allow
            | GuardianActionKind::RecordOnly
            | GuardianActionKind::Warn
            | GuardianActionKind::AskUser
            | GuardianActionKind::Block
    ) {
        return 0.0;
    }

    match ownership {
        OwnershipClass::LauncherManaged => 0.10,
        OwnershipClass::CompositionManaged => 0.25,
        OwnershipClass::ExternalProviderDerived => 0.55,
        OwnershipClass::UserOwned => 0.90,
        OwnershipClass::Unknown => 1.00,
    }
}

fn requires_journal(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Retry
            | GuardianActionKind::Replace
            | GuardianActionKind::Strip
            | GuardianActionKind::Downgrade
            | GuardianActionKind::Degrade
            | GuardianActionKind::Fallback
            | GuardianActionKind::Quarantine
            | GuardianActionKind::Rollback
    )
}

fn ownership_requirement(eligibility: Option<ActionEligibility>) -> OwnershipRequirement {
    eligibility
        .map(|eligibility| eligibility.ownership_requirement)
        .unwrap_or(OwnershipRequirement::Classified)
}

fn action_requires_public_redaction(eligibility: Option<ActionEligibility>) -> bool {
    eligibility
        .map(|eligibility| {
            matches!(
                eligibility.redaction_requirement,
                RedactionRequirement::PublicOutcome
            )
        })
        .unwrap_or(true)
}

fn action_requires_journal(
    action: GuardianActionKind,
    eligibility: Option<ActionEligibility>,
) -> bool {
    let eligibility_requires = eligibility
        .map(|eligibility| match eligibility.journal_requirement {
            JournalRequirement::None => false,
            JournalRequirement::RequiredForAttemptAction => is_attempt_action(action),
            JournalRequirement::RequiredForManagedMutation => is_managed_mutation_action(action),
        })
        .unwrap_or(false);
    eligibility_requires || requires_journal(action)
}

fn is_destructive_mutation(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Replace
            | GuardianActionKind::Quarantine
            | GuardianActionKind::Rollback
    )
}

fn action_is_destructive_mutation(
    action: GuardianActionKind,
    eligibility: Option<ActionEligibility>,
) -> bool {
    let eligibility_marks_destructive = eligibility
        .map(|eligibility| match eligibility.destructive_mutation_risk {
            DestructiveMutationRisk::None => false,
            DestructiveMutationRisk::ManagedMutation
            | DestructiveMutationRisk::UserOrUnknownProtected => is_managed_mutation_action(action),
        })
        .unwrap_or(false);
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
    eligibility: Option<ActionEligibility>,
) -> bool {
    let eligibility_marks_loop_sensitive = eligibility
        .map(|eligibility| match eligibility.retry_loop_sensitivity {
            RetryLoopSensitivity::None | RetryLoopSensitivity::OneAttemptOverride => false,
            RetryLoopSensitivity::RepairAttempt => action == GuardianActionKind::Repair,
            RetryLoopSensitivity::ProviderRetry => action == GuardianActionKind::Retry,
            RetryLoopSensitivity::RepeatedFailureMemory => is_loopable_action(action),
        })
        .unwrap_or(false);
    eligibility_marks_loop_sensitive || is_loopable_action(action)
}

fn action_is_user_intent_sensitive(eligibility: Option<ActionEligibility>) -> bool {
    eligibility
        .map(|eligibility| {
            !matches!(
                eligibility.user_intent_sensitivity,
                UserIntentSensitivity::None
            )
        })
        .unwrap_or(false)
}

fn action_changes_user_intent(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Retry
            | GuardianActionKind::Replace
            | GuardianActionKind::Strip
            | GuardianActionKind::Downgrade
            | GuardianActionKind::Degrade
            | GuardianActionKind::Fallback
            | GuardianActionKind::Quarantine
            | GuardianActionKind::Rollback
    )
}

fn is_attempt_action(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Retry
            | GuardianActionKind::Strip
            | GuardianActionKind::Downgrade
            | GuardianActionKind::Degrade
            | GuardianActionKind::Fallback
    )
}

fn is_managed_mutation_action(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Replace
            | GuardianActionKind::Quarantine
            | GuardianActionKind::Rollback
    )
}

fn launch_intervention_decision_kind(
    intervention: croopor_launcher::GuardianInterventionKind,
) -> GuardianDecisionKind {
    match intervention {
        croopor_launcher::GuardianInterventionKind::SwitchManagedRuntime => {
            GuardianDecisionKind::Fallback
        }
        croopor_launcher::GuardianInterventionKind::StripJvmArgs => GuardianDecisionKind::Strip,
        croopor_launcher::GuardianInterventionKind::DowngradePreset => {
            GuardianDecisionKind::Downgrade
        }
        croopor_launcher::GuardianInterventionKind::DisableCustomGc => GuardianDecisionKind::Strip,
    }
}

fn launch_summary_fallback_message(decision: GuardianDecisionKind) -> &'static str {
    match decision {
        GuardianDecisionKind::Allow => "guardian_allowed",
        GuardianDecisionKind::Warn => "guardian_warned",
        GuardianDecisionKind::Block => "guardian_blocked",
        GuardianDecisionKind::RecordOnly => "guardian_record_only",
        _ => "guardian_intervened",
    }
}

fn sanitize_public_text(value: &str, max_chars: usize) -> Option<String> {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, max_chars)
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianPolicyContext, action_safety_score, decide_guardian_policy,
        decision_pressure_score, launch_summary_decision_kind, launch_summary_safety_outcome,
        policy_reasoning_input,
    };
    use crate::guardian::inference_graph::{
        JournalRequirement, OwnershipRequirement, RetryLoopSensitivity, UserIntentSensitivity,
    };
    use crate::guardian::{
        Diagnosis, DiagnosisId, FactReliability, GuardianActionKind, GuardianConfidence,
        GuardianDecisionKind, GuardianDomain, GuardianFact, GuardianFactId, GuardianImpactVector,
        GuardianMode, GuardianObservation, GuardianSeverity, SafetyCase, diagnose_facts,
        guardian_fact_from_observation,
    };
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
    };

    #[test]
    fn managed_mode_repairs_launcher_managed_corruption() {
        let diagnosis = diagnosis(
            "managed_runtime_corrupt",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::LauncherManaged,
            vec![GuardianActionKind::Repair, GuardianActionKind::Block],
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianDecisionKind::Repair);
        let plan = decision.action_plan.expect("action plan");
        assert_eq!(plan.prerequisite.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(plan.prerequisite.confidence, GuardianConfidence::Confirmed);
        assert_eq!(plan.actions[0].kind, GuardianActionKind::Repair);
    }

    #[test]
    fn malformed_diagnosis_blocks_without_action_plan() {
        let mut diagnosis = diagnosis(
            "missing_target",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::LauncherManaged,
            vec![GuardianActionKind::Repair],
        );
        diagnosis.affected_targets.clear();
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianDecisionKind::Block);
        assert!(decision.action_plan.is_none());
    }

    #[test]
    fn policy_action_plan_sanitizes_prerequisite_targets() {
        let mut diagnosis = diagnosis(
            "managed_runtime_corrupt",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::LauncherManaged,
            vec![GuardianActionKind::Repair],
        );
        diagnosis.affected_targets = vec![TargetDescriptor {
            system: StabilizationSystem::Guardian,
            kind: TargetKind::Runtime,
            id: r"C:\Users\Alice\java.exe --accessToken secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        }];
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
        let diagnosis = diagnosis(
            "jvm_arg_unsupported",
            GuardianSeverity::Blocking,
            GuardianConfidence::Confirmed,
            OwnershipClass::UserOwned,
            vec![
                GuardianActionKind::Strip,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
        );
        let safety_case = safety_case(GuardianMode::Custom, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_explicit_user_intent(),
        );

        assert_eq!(decision.kind, GuardianDecisionKind::AskUser);
    }

    #[test]
    fn disabled_mode_blocks_hard_invariant_even_when_guardian_is_disabled() {
        let diagnosis = diagnosis(
            "user_owned_repair",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::UserOwned,
            vec![GuardianActionKind::Repair, GuardianActionKind::RecordOnly],
        );
        let safety_case = safety_case(GuardianMode::Disabled, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianDecisionKind::Block);
    }

    #[test]
    fn disabled_mode_records_non_blocking_cases_only() {
        let diagnosis = diagnosis(
            "custom_override_present",
            GuardianSeverity::Warning,
            GuardianConfidence::Medium,
            OwnershipClass::UserOwned,
            vec![GuardianActionKind::Warn, GuardianActionKind::RecordOnly],
        );
        let safety_case = safety_case(GuardianMode::Disabled, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianDecisionKind::RecordOnly);
    }

    #[test]
    fn unknown_failure_cushioning_records_only_and_remembers_redacted_target() {
        let raw_target = TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Session,
            id: r"C:\Users\Alice\.minecraft\java.exe --accessToken secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::Unknown,
        };
        let fact = guardian_fact_from_observation(
            GuardianObservation::Unknown("unexpected_native_exit_signal".to_string()),
            OperationPhase::Launching,
            Some(raw_target),
        );
        let diagnoses = diagnose_facts(&[fact], OperationPhase::Launching);
        let diagnosis = diagnoses
            .first()
            .expect("unknown diagnosis should be generated")
            .clone();

        assert_eq!(diagnosis.id.as_str(), "unknown_failure_launching");
        assert_eq!(diagnosis.domain, GuardianDomain::Unknown);
        assert_eq!(diagnosis.confidence, GuardianConfidence::Low);
        assert_eq!(diagnosis.ownership, OwnershipClass::Unknown);
        for destructive in [
            GuardianActionKind::Repair,
            GuardianActionKind::Replace,
            GuardianActionKind::Quarantine,
            GuardianActionKind::Rollback,
        ] {
            assert!(!diagnosis.candidate_actions.contains(&destructive));
        }

        let safety_case = SafetyCase {
            operation_id: None,
            mode: GuardianMode::Managed,
            phase: OperationPhase::Launching,
            diagnoses: vec![diagnosis.clone()],
            hard_constraints: Vec::new(),
        };
        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianDecisionKind::RecordOnly);
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
                .record(
                    GuardianFailureMemoryEntry::observed(
                        diagnosis.id.clone(),
                        diagnosis.domain,
                        safe_target.clone(),
                        GuardianMode::Managed,
                        None,
                        observed_at,
                    )
                    .with_action(
                        GuardianActionKind::RecordOnly,
                        FailureMemoryActionOutcome::NotNeeded,
                    ),
                )
                .expect("record unknown failure memory");
        }

        let memory = failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].occurrence_count, 2);
        assert_eq!(memory[0].repair_attempt_count, 0);
        assert_eq!(
            memory[0].last_action_kind,
            Some(GuardianActionKind::RecordOnly)
        );
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::NotNeeded)
        );
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
        let diagnosis = diagnosis(
            "managed_runtime_corrupt",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::LauncherManaged,
            vec![GuardianActionKind::Repair, GuardianActionKind::Block],
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_missing_journal(),
        );

        assert_eq!(decision.kind, GuardianDecisionKind::Block);
    }

    #[test]
    fn unredacted_public_boundary_blocks_before_action_planning() {
        let diagnosis = diagnosis(
            "managed_runtime_corrupt",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::LauncherManaged,
            vec![GuardianActionKind::Repair, GuardianActionKind::Block],
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_unredacted_public_boundary(),
        );

        assert_eq!(decision.kind, GuardianDecisionKind::Block);
        assert!(decision.action_plan.is_none());
    }

    #[test]
    fn suppression_blocks_repeated_retry_loop() {
        let diagnosis = diagnosis(
            "download_unavailable",
            GuardianSeverity::Blocking,
            GuardianConfidence::Medium,
            OwnershipClass::ExternalProviderDerived,
            vec![
                GuardianActionKind::Retry,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
        );
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_suppression(),
        );

        assert_eq!(decision.kind, GuardianDecisionKind::Block);
    }

    #[test]
    fn performance_fallback_is_preferred_over_block_when_safe() {
        let mut diagnosis = diagnosis(
            "performance_plan_failed",
            GuardianSeverity::Degraded,
            GuardianConfidence::High,
            OwnershipClass::CompositionManaged,
            vec![GuardianActionKind::Fallback, GuardianActionKind::Block],
        );
        diagnosis.domain = GuardianDomain::Performance;
        diagnosis.impact = GuardianImpactVector {
            performance_impact: 0.80,
            launchability_impact: 0.25,
            ..GuardianImpactVector::default()
        };
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let decision =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());

        assert_eq!(decision.kind, GuardianDecisionKind::Fallback);
    }

    #[test]
    fn pressure_and_safety_scores_follow_method_weights() {
        let diagnosis = diagnosis(
            "managed_runtime_corrupt",
            GuardianSeverity::Repairable,
            GuardianConfidence::Confirmed,
            OwnershipClass::LauncherManaged,
            vec![GuardianActionKind::Repair],
        );

        assert!((decision_pressure_score(&diagnosis) - 0.765).abs() < 0.0001);
        assert!(
            action_safety_score(
                &diagnosis,
                GuardianActionKind::Repair,
                GuardianMode::Managed,
                GuardianPolicyContext::current_operation(),
            ) > 0.0
        );
        assert_eq!(
            action_safety_score(
                &diagnosis,
                GuardianActionKind::Repair,
                GuardianMode::Managed,
                GuardianPolicyContext::current_operation().with_suppression(),
            ),
            0.0
        );
    }

    #[test]
    fn policy_reasoning_consumes_graph_evaluation_and_eligibility_inputs() {
        let diagnosis = graph_backed_diagnosis(
            "jvm_args_parse_failed",
            GuardianDomain::Jvm,
            OperationPhase::Validating,
            OwnershipClass::UserOwned,
        );

        let reasoning = policy_reasoning_input(&diagnosis, GuardianActionKind::Strip);
        let graph = reasoning.graph.expect("graph policy input");

        assert_eq!(graph.resolved_severity, diagnosis.severity);
        assert_eq!(graph.resolved_confidence, diagnosis.confidence);
        assert_close(graph.impact_scalar, diagnosis.impact.scalar_severity());
        assert_eq!(
            graph.action_eligibility.journal_requirement,
            JournalRequirement::RequiredForAttemptAction
        );
        assert_eq!(
            graph.action_eligibility.retry_loop_sensitivity,
            RetryLoopSensitivity::OneAttemptOverride
        );
        assert_eq!(
            graph.action_eligibility.user_intent_sensitivity,
            UserIntentSensitivity::ExplicitTechnicalIntent
        );
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
    fn graph_policy_reasoning_truth_table_covers_hard_constraint_inputs() {
        struct Case {
            fact_id: &'static str,
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
                fact_id: "managed_runtime_ready_marker_missing",
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
                fact_id: "artifact_checksum_mismatch",
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
                fact_id: "download_provider_unavailable",
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
                fact_id: "performance_user_owned_conflict",
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
                fact_id: "exit_code_zero",
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
            let diagnosis =
                graph_backed_diagnosis(case.fact_id, case.domain, case.phase, case.ownership);
            let reasoning = policy_reasoning_input(&diagnosis, case.action);

            assert!(
                reasoning.graph.is_some(),
                "{} should be graph-backed",
                case.fact_id
            );
            assert_eq!(
                reasoning.ownership_requirement, case.ownership_requirement,
                "{}",
                case.fact_id
            );
            assert_eq!(
                reasoning.journal_required, case.journal_required,
                "{}",
                case.fact_id
            );
            assert_eq!(
                reasoning.destructive_mutation, case.destructive_mutation,
                "{}",
                case.fact_id
            );
            assert_eq!(
                reasoning.retry_loop_sensitive, case.retry_loop_sensitive,
                "{}",
                case.fact_id
            );
            assert_eq!(
                reasoning.user_intent_sensitive, case.user_intent_sensitive,
                "{}",
                case.fact_id
            );
            assert_eq!(
                reasoning.journal_blocked(
                    GuardianPolicyContext::current_operation().with_missing_journal()
                ),
                case.journal_required,
                "{}",
                case.fact_id
            );
        }
    }

    #[test]
    fn graph_backed_policy_decisions_remain_stable() {
        let runtime_repair = graph_backed_diagnosis(
            "managed_runtime_ready_marker_missing",
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::LauncherManaged,
        );
        assert_eq!(
            decide_guardian_policy(
                &graph_safety_case(GuardianMode::Managed, runtime_repair),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianDecisionKind::Repair
        );

        let jvm_parse = graph_backed_diagnosis(
            "jvm_args_parse_failed",
            GuardianDomain::Jvm,
            OperationPhase::Validating,
            OwnershipClass::UserOwned,
        );
        assert_eq!(
            decide_guardian_policy(
                &graph_safety_case(GuardianMode::Managed, jvm_parse.clone()),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianDecisionKind::Strip
        );
        assert_eq!(
            decide_guardian_policy(
                &graph_safety_case(GuardianMode::Custom, jvm_parse.clone()),
                GuardianPolicyContext::current_operation().with_explicit_user_intent(),
            )
            .kind,
            GuardianDecisionKind::AskUser
        );
        assert_eq!(
            decide_guardian_policy(
                &graph_safety_case(GuardianMode::Disabled, jvm_parse),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianDecisionKind::Block
        );

        let provider_retry = graph_backed_diagnosis(
            "download_provider_unavailable",
            GuardianDomain::Download,
            OperationPhase::Downloading,
            OwnershipClass::ExternalProviderDerived,
        );
        assert_eq!(
            decide_guardian_policy(
                &graph_safety_case(GuardianMode::Managed, provider_retry),
                GuardianPolicyContext::current_operation().with_suppression(),
            )
            .kind,
            GuardianDecisionKind::Block
        );

        let user_owned_artifact = graph_backed_diagnosis(
            "artifact_checksum_mismatch",
            GuardianDomain::Install,
            OperationPhase::Downloading,
            OwnershipClass::UserOwned,
        );
        assert_eq!(
            decide_guardian_policy(
                &graph_safety_case(GuardianMode::Managed, user_owned_artifact),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianDecisionKind::Block
        );
    }

    #[test]
    fn launch_summary_warning_and_block_map_to_policy_outcomes() {
        let mut warned =
            croopor_launcher::GuardianSummary::new(croopor_launcher::GuardianMode::Custom);
        warned.warn_with_guidance(vec![
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.".to_string(),
        ]);
        assert_eq!(
            launch_summary_decision_kind(&warned),
            GuardianDecisionKind::Warn
        );
        assert_eq!(
            launch_summary_safety_outcome(&warned).decision,
            GuardianDecisionKind::Warn
        );

        let mut blocked =
            croopor_launcher::GuardianSummary::new(croopor_launcher::GuardianMode::Managed);
        blocked.block_with_reason_and_guidance(
            "explicit Java override targets Java 8 but this version requires Java 17",
            vec!["Remove the Java override or switch Guardian Mode back to Managed.".to_string()],
        );
        assert_eq!(
            launch_summary_decision_kind(&blocked),
            GuardianDecisionKind::Block
        );
        assert_eq!(
            launch_summary_safety_outcome(&blocked).detail.as_deref(),
            Some("explicit Java override targets Java 8 but this version requires Java 17")
        );
    }

    #[test]
    fn launch_summary_interventions_map_to_specific_policy_actions() {
        let mut summary =
            croopor_launcher::GuardianSummary::new(croopor_launcher::GuardianMode::Managed);
        summary.record_intervention(
            croopor_launcher::GuardianInterventionKind::SwitchManagedRuntime,
            "Guardian switched to managed Java before launch",
            false,
        );

        assert_eq!(
            launch_summary_decision_kind(&summary),
            GuardianDecisionKind::Fallback
        );
    }

    #[test]
    fn launch_summary_unknown_intervention_does_not_overstate_repair() {
        let mut summary =
            croopor_launcher::GuardianSummary::new(croopor_launcher::GuardianMode::Managed);
        summary.decision = croopor_launcher::GuardianDecision::Intervened;

        assert_eq!(
            launch_summary_decision_kind(&summary),
            GuardianDecisionKind::RecordOnly
        );
    }

    #[test]
    fn launch_summary_outcome_redacts_unsafe_details() {
        let mut summary =
            croopor_launcher::GuardianSummary::new(croopor_launcher::GuardianMode::Managed);
        summary.block_with_reason_and_guidance(
            "/home/alice/java.exe --accessToken secret -Xmx8192M",
            vec!["Review the latest game log before retrying.".to_string()],
        );

        let outcome = launch_summary_safety_outcome(&summary);

        assert_eq!(outcome.decision, GuardianDecisionKind::Block);
        assert_eq!(
            outcome.detail.as_deref(),
            Some("Review the latest game log before retrying.")
        );
    }

    fn safety_case(mode: GuardianMode, diagnosis: Diagnosis) -> SafetyCase {
        SafetyCase {
            operation_id: None,
            mode,
            phase: OperationPhase::Preparing,
            diagnoses: vec![diagnosis],
            hard_constraints: Vec::new(),
        }
    }

    fn graph_safety_case(mode: GuardianMode, diagnosis: Diagnosis) -> SafetyCase {
        SafetyCase {
            operation_id: None,
            mode,
            phase: diagnosis.phase,
            diagnoses: vec![diagnosis],
            hard_constraints: Vec::new(),
        }
    }

    fn graph_backed_diagnosis(
        fact_id: &str,
        domain: GuardianDomain,
        phase: OperationPhase,
        ownership: OwnershipClass,
    ) -> Diagnosis {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::new(fact_id),
            domain,
            phase,
            reliability: FactReliability::DirectStructured,
            severity: None,
            confidence: None,
            ownership,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Guardian,
                target_kind_for_domain(domain),
                fact_id,
                ownership,
            )),
            fields: Vec::new(),
        };
        let diagnoses = diagnose_facts(&[fact], phase);
        assert_eq!(diagnoses.len(), 1, "{fact_id}");
        diagnoses
            .into_iter()
            .next()
            .expect("graph diagnosis should exist")
    }

    fn diagnosis(
        id: &str,
        severity: GuardianSeverity,
        confidence: GuardianConfidence,
        ownership: OwnershipClass,
        candidate_actions: Vec<GuardianActionKind>,
    ) -> Diagnosis {
        Diagnosis {
            id: DiagnosisId::new(id),
            domain: GuardianDomain::Runtime,
            severity,
            confidence,
            ownership,
            phase: OperationPhase::Preparing,
            fact_ids: vec![format!("{id}_fact")],
            affected_targets: vec![TargetDescriptor::new(
                StabilizationSystem::Guardian,
                TargetKind::Runtime,
                id,
                ownership,
            )],
            impact: GuardianImpactVector {
                launchability_impact: match severity {
                    GuardianSeverity::Blocking | GuardianSeverity::Critical => 0.95,
                    GuardianSeverity::Repairable | GuardianSeverity::Recoverable => 0.85,
                    GuardianSeverity::Degraded => 0.50,
                    GuardianSeverity::Warning => 0.25,
                    GuardianSeverity::Info => 0.10,
                },
                state_corruption_impact: matches!(
                    severity,
                    GuardianSeverity::Repairable | GuardianSeverity::Recoverable
                )
                .then_some(0.85)
                .unwrap_or(0.0),
                ..GuardianImpactVector::default()
            },
            candidate_actions,
            public_reason_template: id.to_string(),
        }
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

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.0001,
            "expected {actual} to be close to {expected}"
        );
    }
}
