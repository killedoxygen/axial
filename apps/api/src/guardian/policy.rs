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
    severity_score(diagnosis.severity).max(diagnosis.impact.scalar_severity())
        * confidence_score(diagnosis.confidence)
}

pub fn action_safety_score(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    mode: GuardianMode,
    context: GuardianPolicyContext,
) -> f32 {
    let permission = mode_permission(mode, diagnosis, action, context);
    let reversibility = reversibility_score(action);
    let ownership = ownership_risk_for_action(diagnosis.ownership, action);
    let memory = memory_penalty(action, context);
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
            .any(|action| is_loopable_action(*action))
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
    if context.suppression_active && is_loopable_action(action) {
        return Some(CandidateRejection::Suppression);
    }
    if mode_permission(mode, diagnosis, action, context) == 0.0 {
        return Some(CandidateRejection::Mode);
    }
    None
}

fn hard_invariant_rejects(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
) -> bool {
    !context.public_redaction_ready
        || (requires_journal(action) && !context.journal_available)
        || (is_destructive_mutation(action)
            && matches!(
                diagnosis.ownership,
                OwnershipClass::UserOwned | OwnershipClass::Unknown
            ))
}

fn mode_permission(
    mode: GuardianMode,
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
) -> f32 {
    match mode {
        GuardianMode::Managed => {
            if matches!(action, GuardianActionKind::Allow) {
                1.0
            } else {
                1.0
            }
        }
        GuardianMode::Custom => custom_mode_permission(diagnosis, action, context),
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
) -> f32 {
    match action {
        GuardianActionKind::Allow
        | GuardianActionKind::Warn
        | GuardianActionKind::AskUser
        | GuardianActionKind::Block
        | GuardianActionKind::RecordOnly => 1.0,
        GuardianActionKind::Repair => {
            if !context.explicit_user_intent
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
        GuardianActionKind::Fallback | GuardianActionKind::Degrade => (!context
            .explicit_user_intent)
            .then_some(0.75)
            .unwrap_or(0.0),
        GuardianActionKind::Retry => (!context.explicit_user_intent)
            .then_some(0.65)
            .unwrap_or(0.0),
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

fn memory_penalty(action: GuardianActionKind, context: GuardianPolicyContext) -> f32 {
    if context.suppression_active && is_loopable_action(action) {
        1.00
    } else {
        0.0
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

fn is_destructive_mutation(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair
            | GuardianActionKind::Replace
            | GuardianActionKind::Quarantine
            | GuardianActionKind::Rollback
    )
}

fn is_loopable_action(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Retry | GuardianActionKind::Repair
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
    };
    use crate::guardian::{
        Diagnosis, DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDecisionKind,
        GuardianDomain, GuardianImpactVector, GuardianMode, GuardianSeverity, SafetyCase,
    };
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
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
}
