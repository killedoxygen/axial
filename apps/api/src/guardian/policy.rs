use super::rules::rule_order;
use super::{
    ActionPlanPrerequisite, Diagnosis, DiagnosisId, GuardianAction, GuardianActionKind,
    GuardianActionPlan, GuardianFact, GuardianFactId, GuardianMode, GuardianSeverity, SafetyCase,
};
use crate::state::contracts::{OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor};
use serde::Serialize;

#[cfg(test)]
use std::{cell::Cell, future::Future};

#[cfg(test)]
tokio::task_local! {
    static POLICY_EVALUATION_COUNT: Cell<usize>;
}

#[cfg(test)]
pub(crate) async fn with_guardian_policy_evaluation_count<F>(future: F) -> (F::Output, usize)
where
    F: Future,
{
    POLICY_EVALUATION_COUNT
        .scope(Cell::new(0), async move {
            let output = future.await;
            let count = POLICY_EVALUATION_COUNT.with(Cell::get);
            (output, count)
        })
        .await
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianDecision {
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    kind: GuardianActionKind,
    diagnoses: Vec<DiagnosisId>,
    action_plan: Option<GuardianActionPlan>,
}

impl GuardianDecision {
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }

    pub const fn mode(&self) -> GuardianMode {
        self.mode
    }

    pub const fn kind(&self) -> GuardianActionKind {
        self.kind
    }

    pub fn diagnoses(&self) -> &[DiagnosisId] {
        &self.diagnoses
    }

    pub fn action_plan(&self) -> Option<&GuardianActionPlan> {
        self.action_plan.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        operation_id: Option<OperationId>,
        mode: GuardianMode,
        kind: GuardianActionKind,
        diagnoses: Vec<DiagnosisId>,
        action_plan: Option<GuardianActionPlan>,
    ) -> Self {
        Self {
            operation_id,
            mode,
            kind,
            diagnoses,
            action_plan,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianPolicyContext {
    pub journal_available: bool,
    pub suppression_active: bool,
    pub public_redaction_ready: bool,
    pub explicit_user_intent: bool,
    scope: DecisionScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecisionScope {
    General,
    LaunchPreflight {
        admission: PreflightAdmission,
        signals: PreflightSignals,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreflightAdmission {
    Ready,
    Blocked,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PreflightSignals {
    java_fallback: bool,
    jvm_strip: bool,
    warning: bool,
}

impl GuardianPolicyContext {
    pub fn current_operation() -> Self {
        Self {
            journal_available: true,
            suppression_active: false,
            public_redaction_ready: true,
            explicit_user_intent: false,
            scope: DecisionScope::General,
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

    pub(super) fn for_launch_preflight(
        mut self,
        admission: PreflightAdmission,
        facts: &[GuardianFact],
    ) -> Self {
        self.scope = DecisionScope::LaunchPreflight {
            admission,
            signals: PreflightSignals::from_facts(facts),
        };
        self
    }
}

impl PreflightSignals {
    fn from_facts(facts: &[GuardianFact]) -> Self {
        Self {
            java_fallback: facts
                .iter()
                .any(|fact| PREFLIGHT_JAVA_FALLBACK_FACTS.contains(&fact.id)),
            jvm_strip: facts
                .iter()
                .any(|fact| PREFLIGHT_JVM_STRIP_FACTS.contains(&fact.id)),
            warning: facts
                .iter()
                .any(|fact| PREFLIGHT_WARNING_FACTS.contains(&fact.id)),
        }
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
enum PolicyRejection {
    PublicRedactionUnavailable,
    JournalUnavailable,
    ProtectedOwnershipMutation,
    ExplicitUserIntent,
    ActionUnavailableInMode,
    UnknownOwnershipIntervention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModeActionPermission {
    Always,
    CustomAttempt,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModeActionRule {
    action: GuardianActionKind,
    permission: ModeActionPermission,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightModeMatch {
    Any,
    Managed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightActionMatch {
    Any,
    Exact(GuardianActionKind),
    NonBlock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightEvidenceMatch {
    Any,
    AdmissionBlocked,
    Diagnosis(DiagnosisId),
    JavaFallbackSignal,
    JvmStripSignal,
    WarningSignal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightDisposition {
    Exact(GuardianActionKind),
    Incoming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreflightDispositionRule {
    mode: PreflightModeMatch,
    incoming: PreflightActionMatch,
    evidence: PreflightEvidenceMatch,
    disposition: PreflightDisposition,
}

const PREFLIGHT_JAVA_FALLBACK_FACTS: &[GuardianFactId] = &[
    GuardianFactId::JavaOverrideMissing,
    GuardianFactId::JavaOverrideUndefinedSentinel,
    GuardianFactId::JavaProbeFailed,
    GuardianFactId::JavaMajorMismatch,
    GuardianFactId::JavaUpdateTooOld,
];

const PREFLIGHT_JVM_STRIP_FACTS: &[GuardianFactId] = &[
    GuardianFactId::JvmArgsParseFailed,
    GuardianFactId::JvmArgReservedLauncherFlag,
    GuardianFactId::JvmArgMemoryConflict,
    GuardianFactId::JvmArgUnsupportedGc,
    GuardianFactId::JvmArgUnlockOrderInvalid,
    GuardianFactId::JvmArgUnsafeClasspathOverride,
    GuardianFactId::JvmArgUnsafeNativePathOverride,
    GuardianFactId::JvmArgAgentOverride,
];

const PREFLIGHT_WARNING_FACTS: &[GuardianFactId] = &[
    GuardianFactId::JavaOverrideEmpty,
    GuardianFactId::JavaOverrideMissing,
    GuardianFactId::JavaOverrideUndefinedSentinel,
    GuardianFactId::JvmArgsParseFailed,
    GuardianFactId::JvmArgReservedLauncherFlag,
    GuardianFactId::JvmArgMemoryConflict,
    GuardianFactId::JvmArgUnsupportedGc,
    GuardianFactId::JvmArgUnlockOrderInvalid,
    GuardianFactId::JvmArgUnsafeClasspathOverride,
    GuardianFactId::JvmArgUnsafeNativePathOverride,
    GuardianFactId::JvmArgAgentOverride,
    GuardianFactId::LaunchMemoryMinClamped,
    GuardianFactId::LaunchMemoryAllocationLow,
    GuardianFactId::LaunchResourceMemoryPressure,
    GuardianFactId::LaunchResourceCpuPressure,
    GuardianFactId::LaunchResourceInstallPressure,
    GuardianFactId::LaunchResourceDiskPressure,
    GuardianFactId::CustomJavaOverridePresent,
    GuardianFactId::CustomJvmPresetPresent,
    GuardianFactId::CustomJvmArgsPresent,
    GuardianFactId::RecentStartupFailure,
    GuardianFactId::RecentRepairFailed,
    GuardianFactId::RepairSuppressedUntil,
];

const PREFLIGHT_DISPOSITION_RULES: &[PreflightDispositionRule] = &[
    PreflightDispositionRule {
        mode: PreflightModeMatch::Any,
        incoming: PreflightActionMatch::Any,
        evidence: PreflightEvidenceMatch::AdmissionBlocked,
        disposition: PreflightDisposition::Exact(GuardianActionKind::Block),
    },
    PreflightDispositionRule {
        mode: PreflightModeMatch::Any,
        incoming: PreflightActionMatch::Exact(GuardianActionKind::Block),
        evidence: PreflightEvidenceMatch::Any,
        disposition: PreflightDisposition::Exact(GuardianActionKind::Block),
    },
    PreflightDispositionRule {
        mode: PreflightModeMatch::Managed,
        incoming: PreflightActionMatch::Exact(GuardianActionKind::Fallback),
        evidence: PreflightEvidenceMatch::JavaFallbackSignal,
        disposition: PreflightDisposition::Exact(GuardianActionKind::Fallback),
    },
    PreflightDispositionRule {
        mode: PreflightModeMatch::Managed,
        incoming: PreflightActionMatch::Exact(GuardianActionKind::Strip),
        evidence: PreflightEvidenceMatch::JvmStripSignal,
        disposition: PreflightDisposition::Exact(GuardianActionKind::Strip),
    },
    PreflightDispositionRule {
        mode: PreflightModeMatch::Any,
        incoming: PreflightActionMatch::Exact(GuardianActionKind::AskUser),
        evidence: PreflightEvidenceMatch::Diagnosis(DiagnosisId::JavaOverrideUnavailable),
        disposition: PreflightDisposition::Exact(GuardianActionKind::AskUser),
    },
    PreflightDispositionRule {
        mode: PreflightModeMatch::Any,
        incoming: PreflightActionMatch::NonBlock,
        evidence: PreflightEvidenceMatch::WarningSignal,
        disposition: PreflightDisposition::Exact(GuardianActionKind::Warn),
    },
    PreflightDispositionRule {
        mode: PreflightModeMatch::Any,
        incoming: PreflightActionMatch::Any,
        evidence: PreflightEvidenceMatch::Any,
        disposition: PreflightDisposition::Incoming,
    },
];

const MANAGED_MODE_ACTIONS: [ModeActionRule; 11] = [
    mode_action(GuardianActionKind::Allow, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Repair, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Fallback, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Strip, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Downgrade, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Retry, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Quarantine, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Warn, ModeActionPermission::Always),
    mode_action(GuardianActionKind::AskUser, ModeActionPermission::Always),
    mode_action(GuardianActionKind::RecordOnly, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Block, ModeActionPermission::Always),
];

const CUSTOM_MODE_ACTIONS: [ModeActionRule; 11] = [
    mode_action(GuardianActionKind::Allow, ModeActionPermission::Always),
    mode_action(
        GuardianActionKind::Repair,
        ModeActionPermission::Unavailable,
    ),
    mode_action(
        GuardianActionKind::Fallback,
        ModeActionPermission::CustomAttempt,
    ),
    mode_action(GuardianActionKind::Strip, ModeActionPermission::Unavailable),
    mode_action(
        GuardianActionKind::Downgrade,
        ModeActionPermission::Unavailable,
    ),
    mode_action(
        GuardianActionKind::Retry,
        ModeActionPermission::CustomAttempt,
    ),
    mode_action(
        GuardianActionKind::Quarantine,
        ModeActionPermission::Unavailable,
    ),
    mode_action(GuardianActionKind::Warn, ModeActionPermission::Always),
    mode_action(GuardianActionKind::AskUser, ModeActionPermission::Always),
    mode_action(GuardianActionKind::RecordOnly, ModeActionPermission::Always),
    mode_action(GuardianActionKind::Block, ModeActionPermission::Always),
];

const fn mode_action(
    action: GuardianActionKind,
    permission: ModeActionPermission,
) -> ModeActionRule {
    ModeActionRule { action, permission }
}

fn mode_actions(mode: GuardianMode) -> Option<&'static [ModeActionRule; 11]> {
    match mode {
        GuardianMode::Managed => Some(&MANAGED_MODE_ACTIONS),
        GuardianMode::Custom => Some(&CUSTOM_MODE_ACTIONS),
        GuardianMode::Disabled => None,
    }
}

fn public_boundary_rejection(context: GuardianPolicyContext) -> Option<PolicyRejection> {
    (!context.public_redaction_ready).then_some(PolicyRejection::PublicRedactionUnavailable)
}

pub fn decide_guardian_policy(
    safety_case: &SafetyCase,
    context: GuardianPolicyContext,
) -> GuardianDecision {
    #[cfg(test)]
    let _ = POLICY_EVALUATION_COUNT.try_with(|count| count.set(count.get().saturating_add(1)));

    let diagnoses = safety_case
        .diagnoses
        .iter()
        .map(|diagnosis| diagnosis.id())
        .collect::<Vec<_>>();

    if public_boundary_rejection(context).is_some() {
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

    let mut selection = select_policy_action(safety_case.mode, diagnosis, context);
    selection.kind = scoped_disposition(safety_case, selection.kind, context);
    if matches!(context.scope, DecisionScope::LaunchPreflight { .. }) {
        normalize_selected_action(&mut selection.prerequisite, selection.kind);
    }
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

fn scoped_disposition(
    safety_case: &SafetyCase,
    incoming: GuardianActionKind,
    context: GuardianPolicyContext,
) -> GuardianActionKind {
    let DecisionScope::LaunchPreflight { admission, signals } = context.scope else {
        return incoming;
    };
    let rule = PREFLIGHT_DISPOSITION_RULES
        .iter()
        .find(|rule| preflight_rule_matches(**rule, safety_case, incoming, admission, signals))
        .expect("preflight disposition table is total");
    match rule.disposition {
        PreflightDisposition::Exact(disposition) => disposition,
        PreflightDisposition::Incoming => incoming,
    }
}

fn preflight_rule_matches(
    rule: PreflightDispositionRule,
    safety_case: &SafetyCase,
    incoming: GuardianActionKind,
    admission: PreflightAdmission,
    signals: PreflightSignals,
) -> bool {
    let mode_matches = match rule.mode {
        PreflightModeMatch::Any => true,
        PreflightModeMatch::Managed => safety_case.mode == GuardianMode::Managed,
    };
    let action_matches = match rule.incoming {
        PreflightActionMatch::Any => true,
        PreflightActionMatch::Exact(expected) => incoming == expected,
        PreflightActionMatch::NonBlock => incoming != GuardianActionKind::Block,
    };
    let evidence_matches = match rule.evidence {
        PreflightEvidenceMatch::Any => true,
        PreflightEvidenceMatch::AdmissionBlocked => admission == PreflightAdmission::Blocked,
        PreflightEvidenceMatch::Diagnosis(expected) => safety_case
            .diagnoses
            .iter()
            .any(|diagnosis| diagnosis.id() == expected),
        PreflightEvidenceMatch::JavaFallbackSignal => signals.java_fallback,
        PreflightEvidenceMatch::JvmStripSignal => signals.jvm_strip,
        PreflightEvidenceMatch::WarningSignal => signals.warning,
    };
    mode_matches && action_matches && evidence_matches
}

fn normalize_selected_action(
    prerequisite: &mut ActionPlanPrerequisite,
    selected: GuardianActionKind,
) {
    if !prerequisite.candidate_actions.contains(&selected) {
        prerequisite.candidate_actions.push(selected);
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

fn select_policy_action(
    mode: GuardianMode,
    diagnosis: &Diagnosis,
    context: GuardianPolicyContext,
) -> SelectedPolicyAction {
    let prerequisite = public_safe_prerequisite(diagnosis.action_prerequisite());

    let Some(mode_actions) = mode_actions(mode) else {
        return SelectedPolicyAction {
            kind: disabled_mode_action(diagnosis, context),
            prerequisite,
        };
    };

    if context.suppression_active
        && diagnosis
            .candidate_actions()
            .iter()
            .any(|action| action_is_retry_loop_sensitive(*action))
    {
        return SelectedPolicyAction {
            kind: GuardianActionKind::Block,
            prerequisite,
        };
    }

    if mode == GuardianMode::Managed
        && diagnosis
            .candidate_actions()
            .contains(&GuardianActionKind::Repair)
        && hard_invariant_rejection(diagnosis, GuardianActionKind::Repair, context).is_some()
    {
        return SelectedPolicyAction {
            kind: GuardianActionKind::Block,
            prerequisite,
        };
    }

    if (diagnosis.severity() == GuardianSeverity::Info
        || matches!(diagnosis.id(), super::DiagnosisId::UnknownFailure(_)))
        && diagnosis
            .candidate_actions()
            .contains(&GuardianActionKind::RecordOnly)
    {
        return SelectedPolicyAction {
            kind: GuardianActionKind::RecordOnly,
            prerequisite,
        };
    }

    for rule in mode_actions {
        if !diagnosis.candidate_actions().contains(&rule.action) {
            continue;
        }
        if reject_candidate(*rule, diagnosis, context).is_ok() {
            return SelectedPolicyAction {
                kind: rule.action,
                prerequisite,
            };
        }
    }

    SelectedPolicyAction {
        kind: GuardianActionKind::Block,
        prerequisite,
    }
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
    if matches!(
        diagnosis.severity(),
        GuardianSeverity::Blocking | GuardianSeverity::Critical
    ) || diagnosis
        .candidate_actions()
        .iter()
        .any(|action| hard_invariant_rejection(diagnosis, *action, context).is_some())
    {
        GuardianActionKind::Block
    } else {
        GuardianActionKind::RecordOnly
    }
}

fn reject_candidate(
    rule: ModeActionRule,
    diagnosis: &Diagnosis,
    context: GuardianPolicyContext,
) -> Result<(), PolicyRejection> {
    if let Some(rejection) = hard_invariant_rejection(diagnosis, rule.action, context) {
        return Err(rejection);
    }
    match rule.permission {
        ModeActionPermission::Always => {}
        ModeActionPermission::CustomAttempt => {
            if context.explicit_user_intent {
                return Err(PolicyRejection::ExplicitUserIntent);
            }
        }
        ModeActionPermission::Unavailable => {
            return Err(PolicyRejection::ActionUnavailableInMode);
        }
    }
    if diagnosis.ownership() == OwnershipClass::Unknown && action_is_intervention(rule.action) {
        return Err(PolicyRejection::UnknownOwnershipIntervention);
    }
    Ok(())
}

fn hard_invariant_rejection(
    diagnosis: &Diagnosis,
    action: GuardianActionKind,
    context: GuardianPolicyContext,
) -> Option<PolicyRejection> {
    if action_is_intervention(action) && !context.journal_available {
        return Some(PolicyRejection::JournalUnavailable);
    }
    if action_is_destructive_mutation(action)
        && matches!(
            diagnosis.ownership(),
            OwnershipClass::UserOwned | OwnershipClass::Unknown
        )
    {
        return Some(PolicyRejection::ProtectedOwnershipMutation);
    }
    None
}

fn action_is_intervention(action: GuardianActionKind) -> bool {
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

fn action_is_destructive_mutation(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Repair | GuardianActionKind::Quarantine
    )
}

fn action_is_retry_loop_sensitive(action: GuardianActionKind) -> bool {
    matches!(
        action,
        GuardianActionKind::Retry | GuardianActionKind::Repair
    )
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianPolicyContext, ModeActionPermission, PREFLIGHT_DISPOSITION_RULES, PolicyRejection,
        PreflightAdmission, PreflightSignals, action_is_intervention, decide_guardian_policy,
        mode_actions, preflight_rule_matches, public_boundary_rejection, reject_candidate,
        strongest_diagnosis,
    };
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
        let registered_artifact_repair = |coarse_id, coarse_domain, artifact_id| {
            let exact_target = TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "sha256.01234567.89abcdef.01234567.89abcdef.01234567.89abcdef.01234567.89abcdef",
                OwnershipClass::LauncherManaged,
            );
            let mut availability = condition(
                GuardianFactId::RegisteredArtifactRepairAvailable,
                OperationPhase::Launching,
            );
            availability.domain = GuardianDomain::Library;
            availability.ownership = OwnershipClass::LauncherManaged;
            availability.target = Some(exact_target.clone());
            let mut artifact = fact(
                artifact_id,
                GuardianDomain::Library,
                OperationPhase::Launching,
                OwnershipClass::LauncherManaged,
                None,
                None,
            );
            artifact.target = Some(exact_target);
            Case {
                facts: vec![
                    fact(
                        coarse_id,
                        coarse_domain,
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
                    availability,
                    artifact,
                ],
                phase: OperationPhase::Launching,
                expected: DiagnosisId::LauncherManagedArtifactCorrupt,
            }
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
            registered_artifact_repair(
                GuardianFactId::MissingDependency,
                GuardianDomain::Startup,
                GuardianFactId::ArtifactMissing,
            ),
            registered_artifact_repair(
                GuardianFactId::ClasspathModuleConflict,
                GuardianDomain::Startup,
                GuardianFactId::ArtifactMissing,
            ),
            registered_artifact_repair(
                GuardianFactId::LauncherManagedArtifactSignatureCorruption,
                GuardianDomain::Download,
                GuardianFactId::ArtifactHashMismatch,
            ),
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
    fn preflight_disposition_overlaps_resolve_by_declared_order() {
        let java = safety_case(
            GuardianMode::Managed,
            rule_diagnosis(
                GuardianFactId::JavaOverrideMissing,
                GuardianDomain::Runtime,
                OperationPhase::Validating,
                OwnershipClass::UserOwned,
            ),
        );
        let jvm = safety_case(
            GuardianMode::Managed,
            rule_diagnosis(
                GuardianFactId::JvmArgsParseFailed,
                GuardianDomain::Jvm,
                OperationPhase::Validating,
                OwnershipClass::UserOwned,
            ),
        );
        let java_signals = PreflightSignals {
            java_fallback: true,
            warning: true,
            ..PreflightSignals::default()
        };
        let jvm_signals = PreflightSignals {
            jvm_strip: true,
            warning: true,
            ..PreflightSignals::default()
        };

        assert_eq!(
            matching_preflight_rules(
                &jvm,
                GuardianActionKind::Strip,
                PreflightAdmission::Blocked,
                jvm_signals,
            ),
            vec![0, 3, 5, 6]
        );
        assert_eq!(
            matching_preflight_rules(
                &java,
                GuardianActionKind::Fallback,
                PreflightAdmission::Ready,
                java_signals,
            ),
            vec![2, 5, 6]
        );

        let mut custom_java = java.clone();
        custom_java.mode = GuardianMode::Custom;
        assert_eq!(
            matching_preflight_rules(
                &custom_java,
                GuardianActionKind::AskUser,
                PreflightAdmission::Ready,
                java_signals,
            ),
            vec![4, 5, 6]
        );
        let mut custom_jvm = jvm.clone();
        custom_jvm.mode = GuardianMode::Custom;
        assert_eq!(
            matching_preflight_rules(
                &custom_jvm,
                GuardianActionKind::AskUser,
                PreflightAdmission::Ready,
                jvm_signals,
            ),
            vec![5, 6]
        );
        assert_eq!(
            matching_preflight_rules(
                &jvm,
                GuardianActionKind::Block,
                PreflightAdmission::Ready,
                jvm_signals,
            ),
            vec![1, 6]
        );

        let neutral = safety_case(
            GuardianMode::Managed,
            rule_diagnosis(
                GuardianFactId::ExitCodeZero,
                GuardianDomain::Session,
                OperationPhase::Validating,
                OwnershipClass::LauncherManaged,
            ),
        );
        assert_eq!(
            matching_preflight_rules(
                &neutral,
                GuardianActionKind::RecordOnly,
                PreflightAdmission::Ready,
                PreflightSignals::default(),
            ),
            vec![6]
        );
    }

    #[test]
    fn scoped_candidate_normalization_does_not_change_general_candidates() {
        let fact = rule_fact(
            GuardianFactId::JavaOverrideEmpty,
            GuardianDomain::Runtime,
            OperationPhase::Validating,
            OwnershipClass::UserOwned,
        );
        let diagnosis = diagnose(std::slice::from_ref(&fact), OperationPhase::Validating)
            .into_iter()
            .next()
            .expect("Java override diagnosis");
        let original_candidates = diagnosis.candidate_actions().to_vec();
        assert!(!original_candidates.contains(&GuardianActionKind::Warn));
        let safety_case = safety_case(GuardianMode::Managed, diagnosis);

        let general =
            decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
        assert_eq!(general.kind, GuardianActionKind::Fallback);
        assert_eq!(
            general
                .action_plan
                .expect("general action plan")
                .prerequisite
                .candidate_actions,
            original_candidates
        );

        let scoped = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation()
                .for_launch_preflight(PreflightAdmission::Ready, &[fact]),
        );
        assert_eq!(scoped.kind, GuardianActionKind::Warn);
        let scoped_candidates = scoped
            .action_plan
            .expect("scoped action plan")
            .prerequisite
            .candidate_actions;
        assert_eq!(
            scoped_candidates[..original_candidates.len()],
            original_candidates
        );
        assert_eq!(scoped_candidates.last(), Some(&GuardianActionKind::Warn));
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
    fn custom_corruption_offer_does_not_require_mutation_admission() {
        let diagnosis = rule_diagnosis(
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::LauncherManaged,
        );
        let safety_case = safety_case(GuardianMode::Custom, diagnosis);

        let decision = decide_guardian_policy(
            &safety_case,
            GuardianPolicyContext::current_operation().with_missing_journal(),
        );

        assert_eq!(decision.kind, GuardianActionKind::AskUser);
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
    fn mode_action_rules_are_total_and_encode_action_preference() {
        let expected_order = [
            GuardianActionKind::Allow,
            GuardianActionKind::Repair,
            GuardianActionKind::Fallback,
            GuardianActionKind::Strip,
            GuardianActionKind::Downgrade,
            GuardianActionKind::Retry,
            GuardianActionKind::Quarantine,
            GuardianActionKind::Warn,
            GuardianActionKind::AskUser,
            GuardianActionKind::RecordOnly,
            GuardianActionKind::Block,
        ];

        for mode in [GuardianMode::Managed, GuardianMode::Custom] {
            let rules = mode_actions(mode).expect("active mode action rules");
            assert_eq!(rules.map(|rule| rule.action), expected_order, "{mode:?}");
            let expected_permissions = match mode {
                GuardianMode::Managed => [ModeActionPermission::Always; 11],
                GuardianMode::Custom => [
                    ModeActionPermission::Always,
                    ModeActionPermission::Unavailable,
                    ModeActionPermission::CustomAttempt,
                    ModeActionPermission::Unavailable,
                    ModeActionPermission::Unavailable,
                    ModeActionPermission::CustomAttempt,
                    ModeActionPermission::Unavailable,
                    ModeActionPermission::Always,
                    ModeActionPermission::Always,
                    ModeActionPermission::Always,
                    ModeActionPermission::Always,
                ],
                GuardianMode::Disabled => unreachable!("Disabled has a disposition rule"),
            };
            assert_eq!(
                rules.map(|rule| rule.permission),
                expected_permissions,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn mode_action_ownership_context_truth_table_has_typed_rejections() {
        let ownerships = [
            OwnershipClass::LauncherManaged,
            OwnershipClass::CompositionManaged,
            OwnershipClass::UserOwned,
            OwnershipClass::ExternalProviderDerived,
            OwnershipClass::Unknown,
        ];

        for mode in [GuardianMode::Managed, GuardianMode::Custom] {
            for ownership in ownerships {
                let diagnosis = rule_diagnosis(
                    GuardianFactId::CustomJavaOverridePresent,
                    GuardianDomain::Runtime,
                    OperationPhase::Preparing,
                    ownership,
                );
                for rule in mode_actions(mode).expect("active mode action rules") {
                    for journal_available in [false, true] {
                        for suppression_active in [false, true] {
                            for explicit_user_intent in [false, true] {
                                let mut context = GuardianPolicyContext::current_operation();
                                context.journal_available = journal_available;
                                context.suppression_active = suppression_active;
                                context.explicit_user_intent = explicit_user_intent;
                                let actual = reject_candidate(*rule, &diagnosis, context).err();
                                let expected =
                                    expected_rejection(mode, rule.action, ownership, context);
                                assert_eq!(
                                    actual, expected,
                                    "{mode:?} {:?} {ownership:?} {context:?}",
                                    rule.action
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn explicit_preference_ignores_candidate_declaration_order() {
        let artifact = rule_diagnosis(
            GuardianFactId::ArtifactChecksumMismatch,
            GuardianDomain::Install,
            OperationPhase::Downloading,
            OwnershipClass::LauncherManaged,
        );
        let performance = rule_diagnosis(
            GuardianFactId::PerformanceRulesInvalid,
            GuardianDomain::Performance,
            OperationPhase::Planning,
            OwnershipClass::CompositionManaged,
        );

        assert_eq!(
            decide_guardian_policy(
                &safety_case(GuardianMode::Managed, artifact),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianActionKind::Repair
        );
        assert_eq!(
            decide_guardian_policy(
                &safety_case(GuardianMode::Managed, performance),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianActionKind::Warn
        );
    }

    #[test]
    fn safe_fallbacks_survive_rejected_interventions() {
        let malformed_args = rule_diagnosis(
            GuardianFactId::JvmArgsParseFailed,
            GuardianDomain::Jvm,
            OperationPhase::Validating,
            OwnershipClass::Unknown,
        );
        let unavailable_java = rule_diagnosis(
            GuardianFactId::JavaOverrideMissing,
            GuardianDomain::Runtime,
            OperationPhase::Preparing,
            OwnershipClass::UserOwned,
        );

        assert_eq!(
            decide_guardian_policy(
                &safety_case(GuardianMode::Managed, malformed_args),
                GuardianPolicyContext::current_operation(),
            )
            .kind,
            GuardianActionKind::AskUser
        );
        assert_eq!(
            decide_guardian_policy(
                &safety_case(GuardianMode::Managed, unavailable_java),
                GuardianPolicyContext::current_operation().with_missing_journal(),
            )
            .kind,
            GuardianActionKind::AskUser
        );
    }

    #[test]
    fn public_boundary_rejection_precedes_planning_for_every_mode() {
        let rejected = GuardianPolicyContext::current_operation()
            .with_missing_journal()
            .with_unredacted_public_boundary();
        assert_eq!(
            public_boundary_rejection(rejected),
            Some(PolicyRejection::PublicRedactionUnavailable)
        );

        for mode in [
            GuardianMode::Managed,
            GuardianMode::Custom,
            GuardianMode::Disabled,
        ] {
            let safety_case = SafetyCase {
                operation_id: None,
                mode,
                phase: OperationPhase::Preparing,
                diagnoses: Vec::new(),
            };
            let decision = decide_guardian_policy(&safety_case, rejected);
            assert_eq!(decision.kind, GuardianActionKind::Block, "{mode:?}");
            assert!(decision.action_plan.is_none(), "{mode:?}");

            let decision =
                decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
            assert_eq!(decision.kind, GuardianActionKind::Allow, "{mode:?}");
            assert!(decision.action_plan.is_none(), "{mode:?}");
        }
    }

    #[test]
    fn disabled_disposition_is_total_over_ownership_and_context() {
        let ownerships = [
            OwnershipClass::LauncherManaged,
            OwnershipClass::CompositionManaged,
            OwnershipClass::UserOwned,
            OwnershipClass::ExternalProviderDerived,
            OwnershipClass::Unknown,
        ];

        for ownership in ownerships {
            for journal_available in [false, true] {
                for suppression_active in [false, true] {
                    for public_redaction_ready in [false, true] {
                        for explicit_user_intent in [false, true] {
                            let mut context = GuardianPolicyContext::current_operation();
                            context.journal_available = journal_available;
                            context.suppression_active = suppression_active;
                            context.public_redaction_ready = public_redaction_ready;
                            context.explicit_user_intent = explicit_user_intent;
                            let warning = rule_diagnosis(
                                GuardianFactId::CustomJavaOverridePresent,
                                GuardianDomain::Runtime,
                                OperationPhase::Preparing,
                                ownership,
                            );
                            let repairable = rule_diagnosis(
                                GuardianFactId::ManagedRuntimeReadyMarkerMissing,
                                GuardianDomain::Runtime,
                                OperationPhase::Preparing,
                                ownership,
                            );
                            let blocking = rule_diagnosis(
                                GuardianFactId::JavaOverrideMissing,
                                GuardianDomain::Runtime,
                                OperationPhase::Preparing,
                                ownership,
                            );

                            let warning_decision = decide_guardian_policy(
                                &safety_case(GuardianMode::Disabled, warning),
                                context,
                            );
                            let repair_decision = decide_guardian_policy(
                                &safety_case(GuardianMode::Disabled, repairable),
                                context,
                            );
                            let blocking_decision = decide_guardian_policy(
                                &safety_case(GuardianMode::Disabled, blocking),
                                context,
                            );
                            let outer_block = !public_redaction_ready;
                            assert_eq!(
                                warning_decision.kind,
                                if outer_block {
                                    GuardianActionKind::Block
                                } else {
                                    GuardianActionKind::RecordOnly
                                },
                                "warning {ownership:?} {context:?}"
                            );
                            assert_eq!(
                                repair_decision.kind,
                                if outer_block
                                    || !journal_available
                                    || matches!(
                                        ownership,
                                        OwnershipClass::UserOwned | OwnershipClass::Unknown
                                    )
                                {
                                    GuardianActionKind::Block
                                } else {
                                    GuardianActionKind::RecordOnly
                                },
                                "repairable {ownership:?} {context:?}"
                            );
                            assert_eq!(
                                blocking_decision.kind,
                                GuardianActionKind::Block,
                                "blocking {ownership:?} {context:?}"
                            );
                            for decision in [warning_decision, repair_decision, blocking_decision] {
                                assert_eq!(
                                    decision.action_plan.is_none(),
                                    outer_block,
                                    "{ownership:?} {context:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn expected_rejection(
        mode: GuardianMode,
        action: GuardianActionKind,
        ownership: OwnershipClass,
        context: GuardianPolicyContext,
    ) -> Option<PolicyRejection> {
        let intervention = matches!(
            action,
            GuardianActionKind::Repair
                | GuardianActionKind::Retry
                | GuardianActionKind::Strip
                | GuardianActionKind::Downgrade
                | GuardianActionKind::Fallback
                | GuardianActionKind::Quarantine
        );
        if intervention && !context.journal_available {
            return Some(PolicyRejection::JournalUnavailable);
        }
        if matches!(
            action,
            GuardianActionKind::Repair | GuardianActionKind::Quarantine
        ) && matches!(
            ownership,
            OwnershipClass::UserOwned | OwnershipClass::Unknown
        ) {
            return Some(PolicyRejection::ProtectedOwnershipMutation);
        }

        let permission_rejection = match (mode, action) {
            (GuardianMode::Managed, _) => None,
            (GuardianMode::Custom, GuardianActionKind::Fallback | GuardianActionKind::Retry)
                if context.explicit_user_intent =>
            {
                Some(PolicyRejection::ExplicitUserIntent)
            }
            (
                GuardianMode::Custom,
                GuardianActionKind::Repair
                | GuardianActionKind::Strip
                | GuardianActionKind::Downgrade
                | GuardianActionKind::Quarantine,
            ) => Some(PolicyRejection::ActionUnavailableInMode),
            (GuardianMode::Custom, _) => None,
            (GuardianMode::Disabled, _) => unreachable!("Disabled has a disposition rule"),
        };
        if permission_rejection.is_some() {
            return permission_rejection;
        }
        (ownership == OwnershipClass::Unknown && action_is_intervention(action))
            .then_some(PolicyRejection::UnknownOwnershipIntervention)
    }

    fn matching_preflight_rules(
        safety_case: &SafetyCase,
        incoming: GuardianActionKind,
        admission: PreflightAdmission,
        signals: PreflightSignals,
    ) -> Vec<usize> {
        PREFLIGHT_DISPOSITION_RULES
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| {
                preflight_rule_matches(*rule, safety_case, incoming, admission, signals)
                    .then_some(index)
            })
            .collect()
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
        let fact = rule_fact(fact_id, domain, phase, ownership);
        let diagnoses = diagnose(&[fact], phase);
        assert_eq!(diagnoses.len(), 1, "{}", fact_id.as_str());
        diagnoses
            .into_iter()
            .next()
            .expect("rule diagnosis should exist")
    }

    fn rule_fact(
        fact_id: GuardianFactId,
        domain: GuardianDomain,
        phase: OperationPhase,
        ownership: OwnershipClass,
    ) -> GuardianFact {
        GuardianFact {
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
}
