//! Guardian repair planning.
//!
//! This module builds bounded repair plans from Guardian decisions. It does
//! not execute filesystem or network effects.

use super::{
    DiagnosisId, GuardianActionKind, GuardianActionPlan, GuardianDecision, GuardianDecisionKind,
};
use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_evidence_token};
use crate::state::contracts::{OperationPhase, OwnershipClass, TargetDescriptor, TargetKind};
use serde::{Deserialize, Serialize};

const CORRUPT_ARTIFACT_DIAGNOSIS: &str = "launcher_managed_artifact_corrupt";
const ARTIFACT_REPAIR_MAX_ATTEMPTS: u32 = 1;
const ARTIFACT_REPAIR_MAX_DEPTH: u8 = 3;
const ARTIFACT_REPAIR_SUPPRESSION_KEY: &str = "install:launcher_managed_artifact_corrupt";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianRepairPlanningContext {
    pub journal_available: bool,
    pub executor_available: bool,
    pub suppression_active: bool,
    pub public_redaction_ready: bool,
}

impl GuardianRepairPlanningContext {
    pub fn current_operation() -> Self {
        Self {
            journal_available: true,
            executor_available: true,
            suppression_active: false,
            public_redaction_ready: true,
        }
    }

    pub fn with_missing_journal(mut self) -> Self {
        self.journal_available = false;
        self
    }

    pub fn with_missing_executor(mut self) -> Self {
        self.executor_available = false;
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
}

impl Default for GuardianRepairPlanningContext {
    fn default() -> Self {
        Self::current_operation()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianRepairPlan {
    pub diagnosis_id: DiagnosisId,
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub max_depth: u8,
    pub max_attempts: u32,
    pub suppression_key: String,
    pub public_summary: String,
    pub tasks: Vec<GuardianRepairTask>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianRepairTask {
    pub id: String,
    pub kind: GuardianRepairTaskKind,
    pub action: GuardianActionKind,
    pub phase: OperationPhase,
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub executor: GuardianRepairExecutor,
    pub mutation: GuardianRepairMutation,
    pub reversibility: GuardianRepairReversibility,
    pub max_attempts: u32,
    pub requires_user_confirmation: bool,
    pub public_summary: String,
    pub private_evidence_refs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianRepairTaskKind {
    JournalRepairStart,
    QuarantineLauncherManagedTarget,
    DownloadArtifactToTemp,
    VerifyArtifactChecksum,
    PromoteVerifiedArtifact,
    RecordRepairOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianRepairExecutor {
    StateJournal,
    ExecutionFileQuarantine,
    ExecutionDownload,
    ExecutionArtifactVerifier,
    ExecutionFilePromotion,
    GuardianOutcomeRecorder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianRepairMutation {
    None,
    QuarantineManagedTarget,
    WriteTempArtifact,
    PromoteVerifiedArtifact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianRepairReversibility {
    JournalOnly,
    QuarantineOnly,
    RepairByRedownload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianRepairPlanRejection {
    NonRepairDecision,
    MissingActionPlan,
    UnsupportedDiagnosis,
    MissingTarget,
    UnsafeOwnership,
    MissingJournal,
    MissingExecutorCapability,
    Suppressed,
    UnsafePublicBoundary,
}

pub fn plan_launcher_managed_artifact_repair(
    decision: &GuardianDecision,
    context: GuardianRepairPlanningContext,
) -> Result<GuardianRepairPlan, GuardianRepairPlanRejection> {
    plan_launcher_managed_artifact_repair_with_tasks(decision, context, artifact_repair_tasks)
}

pub fn plan_launcher_managed_missing_artifact_repair(
    decision: &GuardianDecision,
    context: GuardianRepairPlanningContext,
) -> Result<GuardianRepairPlan, GuardianRepairPlanRejection> {
    plan_launcher_managed_artifact_repair_with_tasks(
        decision,
        context,
        missing_artifact_repair_tasks,
    )
}

fn plan_launcher_managed_artifact_repair_with_tasks(
    decision: &GuardianDecision,
    context: GuardianRepairPlanningContext,
    tasks: fn(&DiagnosisId, &TargetDescriptor) -> Vec<GuardianRepairTask>,
) -> Result<GuardianRepairPlan, GuardianRepairPlanRejection> {
    if !context.public_redaction_ready {
        return Err(GuardianRepairPlanRejection::UnsafePublicBoundary);
    }
    if !context.journal_available {
        return Err(GuardianRepairPlanRejection::MissingJournal);
    }
    if context.suppression_active {
        return Err(GuardianRepairPlanRejection::Suppressed);
    }
    if !context.executor_available {
        return Err(GuardianRepairPlanRejection::MissingExecutorCapability);
    }
    if decision.kind != GuardianDecisionKind::Repair {
        return Err(GuardianRepairPlanRejection::NonRepairDecision);
    }

    let plan = decision
        .action_plan
        .as_ref()
        .ok_or(GuardianRepairPlanRejection::MissingActionPlan)?;
    let diagnosis_id = supported_artifact_diagnosis(decision, plan)?;
    let target = repair_target(plan)?;
    let target = public_safe_target(&target)?;

    if target.ownership != OwnershipClass::LauncherManaged {
        return Err(GuardianRepairPlanRejection::UnsafeOwnership);
    }
    if !matches!(target.kind, TargetKind::Artifact | TargetKind::Version) {
        return Err(GuardianRepairPlanRejection::UnsupportedDiagnosis);
    }

    Ok(GuardianRepairPlan {
        diagnosis_id: diagnosis_id.clone(),
        target: target.clone(),
        ownership: target.ownership,
        max_depth: ARTIFACT_REPAIR_MAX_DEPTH,
        max_attempts: ARTIFACT_REPAIR_MAX_ATTEMPTS,
        suppression_key: safe_fragment(
            &format!("{ARTIFACT_REPAIR_SUPPRESSION_KEY}:{}", target.id),
            "repair_suppression",
        ),
        public_summary: public_summary("repair_launcher_managed_artifact"),
        tasks: tasks(&diagnosis_id, &target),
    })
}

fn supported_artifact_diagnosis(
    decision: &GuardianDecision,
    plan: &GuardianActionPlan,
) -> Result<DiagnosisId, GuardianRepairPlanRejection> {
    if plan.prerequisite.diagnosis_id.as_str() != CORRUPT_ARTIFACT_DIAGNOSIS
        || !decision
            .diagnoses
            .iter()
            .any(|diagnosis| diagnosis.as_str() == CORRUPT_ARTIFACT_DIAGNOSIS)
        || !plan
            .prerequisite
            .candidate_actions
            .contains(&GuardianActionKind::Repair)
    {
        return Err(GuardianRepairPlanRejection::UnsupportedDiagnosis);
    }
    Ok(plan.prerequisite.diagnosis_id.clone())
}

fn repair_target(
    plan: &GuardianActionPlan,
) -> Result<TargetDescriptor, GuardianRepairPlanRejection> {
    plan.actions
        .iter()
        .find(|action| action.kind == GuardianActionKind::Repair)
        .and_then(|action| action.target.clone())
        .or_else(|| plan.prerequisite.affected_targets.first().cloned())
        .ok_or(GuardianRepairPlanRejection::MissingTarget)
}

fn artifact_repair_tasks(
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
) -> Vec<GuardianRepairTask> {
    vec![
        repair_task(
            "journal_repair_start",
            GuardianRepairTaskKind::JournalRepairStart,
            GuardianActionKind::RecordOnly,
            GuardianRepairExecutor::StateJournal,
            GuardianRepairMutation::None,
            GuardianRepairReversibility::JournalOnly,
            "journal_guardian_artifact_repair_start",
            diagnosis_id,
            target,
        ),
        repair_task(
            "quarantine_launcher_managed_target",
            GuardianRepairTaskKind::QuarantineLauncherManagedTarget,
            GuardianActionKind::Quarantine,
            GuardianRepairExecutor::ExecutionFileQuarantine,
            GuardianRepairMutation::QuarantineManagedTarget,
            GuardianRepairReversibility::QuarantineOnly,
            "quarantine_corrupt_launcher_managed_artifact",
            diagnosis_id,
            target,
        ),
        repair_task(
            "download_artifact_to_temp",
            GuardianRepairTaskKind::DownloadArtifactToTemp,
            GuardianActionKind::Repair,
            GuardianRepairExecutor::ExecutionDownload,
            GuardianRepairMutation::WriteTempArtifact,
            GuardianRepairReversibility::RepairByRedownload,
            "download_replacement_artifact_to_temp",
            diagnosis_id,
            target,
        ),
        repair_task(
            "verify_artifact_checksum",
            GuardianRepairTaskKind::VerifyArtifactChecksum,
            GuardianActionKind::Repair,
            GuardianRepairExecutor::ExecutionArtifactVerifier,
            GuardianRepairMutation::None,
            GuardianRepairReversibility::RepairByRedownload,
            "verify_replacement_artifact_checksum",
            diagnosis_id,
            target,
        ),
        repair_task(
            "promote_verified_artifact",
            GuardianRepairTaskKind::PromoteVerifiedArtifact,
            GuardianActionKind::Repair,
            GuardianRepairExecutor::ExecutionFilePromotion,
            GuardianRepairMutation::PromoteVerifiedArtifact,
            GuardianRepairReversibility::RepairByRedownload,
            "promote_verified_replacement_artifact",
            diagnosis_id,
            target,
        ),
        repair_task(
            "record_repair_outcome",
            GuardianRepairTaskKind::RecordRepairOutcome,
            GuardianActionKind::RecordOnly,
            GuardianRepairExecutor::GuardianOutcomeRecorder,
            GuardianRepairMutation::None,
            GuardianRepairReversibility::JournalOnly,
            "record_guardian_artifact_repair_outcome",
            diagnosis_id,
            target,
        ),
    ]
}

fn missing_artifact_repair_tasks(
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
) -> Vec<GuardianRepairTask> {
    vec![
        repair_task(
            "journal_repair_start",
            GuardianRepairTaskKind::JournalRepairStart,
            GuardianActionKind::RecordOnly,
            GuardianRepairExecutor::StateJournal,
            GuardianRepairMutation::None,
            GuardianRepairReversibility::JournalOnly,
            "journal_guardian_artifact_repair_start",
            diagnosis_id,
            target,
        ),
        repair_task(
            "download_artifact_to_temp",
            GuardianRepairTaskKind::DownloadArtifactToTemp,
            GuardianActionKind::Repair,
            GuardianRepairExecutor::ExecutionDownload,
            GuardianRepairMutation::WriteTempArtifact,
            GuardianRepairReversibility::RepairByRedownload,
            "download_replacement_artifact_to_temp",
            diagnosis_id,
            target,
        ),
        repair_task(
            "verify_artifact_checksum",
            GuardianRepairTaskKind::VerifyArtifactChecksum,
            GuardianActionKind::Repair,
            GuardianRepairExecutor::ExecutionArtifactVerifier,
            GuardianRepairMutation::None,
            GuardianRepairReversibility::RepairByRedownload,
            "verify_replacement_artifact_checksum",
            diagnosis_id,
            target,
        ),
        repair_task(
            "promote_verified_artifact",
            GuardianRepairTaskKind::PromoteVerifiedArtifact,
            GuardianActionKind::Repair,
            GuardianRepairExecutor::ExecutionFilePromotion,
            GuardianRepairMutation::PromoteVerifiedArtifact,
            GuardianRepairReversibility::RepairByRedownload,
            "promote_verified_replacement_artifact",
            diagnosis_id,
            target,
        ),
        repair_task(
            "record_repair_outcome",
            GuardianRepairTaskKind::RecordRepairOutcome,
            GuardianActionKind::RecordOnly,
            GuardianRepairExecutor::GuardianOutcomeRecorder,
            GuardianRepairMutation::None,
            GuardianRepairReversibility::JournalOnly,
            "record_guardian_artifact_repair_outcome",
            diagnosis_id,
            target,
        ),
    ]
}

fn repair_task(
    id: &str,
    kind: GuardianRepairTaskKind,
    action: GuardianActionKind,
    executor: GuardianRepairExecutor,
    mutation: GuardianRepairMutation,
    reversibility: GuardianRepairReversibility,
    summary: &str,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
) -> GuardianRepairTask {
    GuardianRepairTask {
        id: safe_fragment(id, "repair_task"),
        kind,
        action,
        phase: OperationPhase::Repairing,
        target: target.clone(),
        ownership: target.ownership,
        executor,
        mutation,
        reversibility,
        max_attempts: ARTIFACT_REPAIR_MAX_ATTEMPTS,
        requires_user_confirmation: false,
        public_summary: public_summary(summary),
        private_evidence_refs: vec![safe_fragment(
            &format!("diagnosis:{}", diagnosis_id.as_str()),
            "evidence_ref",
        )],
    }
}

fn public_safe_target(
    target: &TargetDescriptor,
) -> Result<TargetDescriptor, GuardianRepairPlanRejection> {
    let sanitized = TargetDescriptor::new(target.system, target.kind, &target.id, target.ownership);
    if sanitized.id != target.id {
        return Err(GuardianRepairPlanRejection::UnsafePublicBoundary);
    }
    Ok(sanitized)
}

fn public_summary(value: &str) -> String {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, 160)
        .unwrap_or_else(|| "guardian_repair_step".to_string())
}

fn safe_fragment(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianRepairExecutor, GuardianRepairMutation, GuardianRepairPlanRejection,
        GuardianRepairPlanningContext, GuardianRepairTaskKind,
        plan_launcher_managed_artifact_repair, plan_launcher_managed_missing_artifact_repair,
    };
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDecision, GuardianDecisionKind,
        GuardianMode,
    };
    use crate::state::contracts::{
        OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn plans_launcher_managed_artifact_repair_workflow() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let plan = plan_launcher_managed_artifact_repair(
            &decision,
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect("repair plan");

        assert_eq!(
            plan.diagnosis_id.as_str(),
            "launcher_managed_artifact_corrupt"
        );
        assert_eq!(plan.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(plan.max_attempts, 1);
        assert_eq!(plan.max_depth, 3);
        assert_eq!(plan.tasks.len(), 6);
        assert_eq!(
            plan.tasks.iter().map(|task| task.kind).collect::<Vec<_>>(),
            vec![
                GuardianRepairTaskKind::JournalRepairStart,
                GuardianRepairTaskKind::QuarantineLauncherManagedTarget,
                GuardianRepairTaskKind::DownloadArtifactToTemp,
                GuardianRepairTaskKind::VerifyArtifactChecksum,
                GuardianRepairTaskKind::PromoteVerifiedArtifact,
                GuardianRepairTaskKind::RecordRepairOutcome,
            ]
        );
        assert_eq!(
            plan.tasks[1].executor,
            GuardianRepairExecutor::ExecutionFileQuarantine
        );
        assert_eq!(
            plan.tasks[1].mutation,
            GuardianRepairMutation::QuarantineManagedTarget
        );
        assert!(
            plan.tasks
                .iter()
                .all(|task| !task.requires_user_confirmation)
        );
    }

    #[test]
    fn plans_launcher_managed_missing_artifact_repair_without_quarantine() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let plan = plan_launcher_managed_missing_artifact_repair(
            &decision,
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect("missing repair plan");

        assert_eq!(
            plan.diagnosis_id.as_str(),
            "launcher_managed_artifact_corrupt"
        );
        assert_eq!(plan.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(plan.tasks.len(), 5);
        assert_eq!(
            plan.tasks.iter().map(|task| task.kind).collect::<Vec<_>>(),
            vec![
                GuardianRepairTaskKind::JournalRepairStart,
                GuardianRepairTaskKind::DownloadArtifactToTemp,
                GuardianRepairTaskKind::VerifyArtifactChecksum,
                GuardianRepairTaskKind::PromoteVerifiedArtifact,
                GuardianRepairTaskKind::RecordRepairOutcome,
            ]
        );
        assert!(
            !plan
                .tasks
                .iter()
                .any(|task| task.kind == GuardianRepairTaskKind::QuarantineLauncherManagedTarget)
        );
    }

    #[test]
    fn rejects_user_owned_and_unknown_artifact_repair() {
        for ownership in [OwnershipClass::UserOwned, OwnershipClass::Unknown] {
            let decision = repair_decision(ownership);

            let error = plan_launcher_managed_artifact_repair(
                &decision,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("unsafe ownership should reject");

            assert_eq!(error, GuardianRepairPlanRejection::UnsafeOwnership);
        }
    }

    #[test]
    fn rejects_missing_journal_executor_and_suppression() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &decision,
                GuardianRepairPlanningContext::current_operation().with_missing_journal(),
            )
            .expect_err("missing journal"),
            GuardianRepairPlanRejection::MissingJournal
        );
        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &decision,
                GuardianRepairPlanningContext::current_operation().with_missing_executor(),
            )
            .expect_err("missing executor"),
            GuardianRepairPlanRejection::MissingExecutorCapability
        );
        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &decision,
                GuardianRepairPlanningContext::current_operation().with_suppression(),
            )
            .expect_err("suppressed"),
            GuardianRepairPlanRejection::Suppressed
        );
    }

    #[test]
    fn rejects_non_repair_and_unsupported_diagnosis() {
        let mut non_repair = repair_decision(OwnershipClass::LauncherManaged);
        non_repair.kind = GuardianDecisionKind::Block;

        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &non_repair,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("non repair"),
            GuardianRepairPlanRejection::NonRepairDecision
        );

        let mut unsupported = repair_decision(OwnershipClass::LauncherManaged);
        unsupported.diagnoses = vec![DiagnosisId::new("managed_runtime_corrupt")];
        unsupported
            .action_plan
            .as_mut()
            .expect("plan")
            .prerequisite
            .diagnosis_id = DiagnosisId::new("managed_runtime_corrupt");

        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &unsupported,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("unsupported"),
            GuardianRepairPlanRejection::UnsupportedDiagnosis
        );
    }

    #[test]
    fn rejects_missing_target_and_unredacted_public_boundary() {
        let mut missing_target = repair_decision(OwnershipClass::LauncherManaged);
        let plan = missing_target.action_plan.as_mut().expect("plan");
        plan.actions[0].target = None;
        plan.prerequisite.affected_targets.clear();

        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &missing_target,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("missing target"),
            GuardianRepairPlanRejection::MissingTarget
        );

        let mut unsafe_target = repair_decision(OwnershipClass::LauncherManaged);
        let raw_target = TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Artifact,
            id: r"C:\Users\Alice\.minecraft\libraries\bad.jar token=secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        };
        let plan = unsafe_target.action_plan.as_mut().expect("plan");
        plan.actions[0].target = Some(raw_target.clone());
        plan.prerequisite.affected_targets = vec![raw_target];

        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &unsafe_target,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("unsafe public target"),
            GuardianRepairPlanRejection::UnsafePublicBoundary
        );

        assert_eq!(
            plan_launcher_managed_artifact_repair(
                &repair_decision(OwnershipClass::LauncherManaged),
                GuardianRepairPlanningContext::current_operation()
                    .with_unredacted_public_boundary(),
            )
            .expect_err("unredacted boundary"),
            GuardianRepairPlanRejection::UnsafePublicBoundary
        );
    }

    #[test]
    fn planned_output_is_public_safe() {
        let plan = plan_launcher_managed_artifact_repair(
            &repair_decision(OwnershipClass::LauncherManaged),
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect("repair plan");
        let encoded = serde_json::to_string(&plan).expect("plan json");
        let lower = encoded.to_ascii_lowercase();

        assert!(!lower.contains("users"));
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("token"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("secret"));
    }

    fn repair_decision(ownership: OwnershipClass) -> GuardianDecision {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "libraries_com_example_bad-1.0.jar",
            ownership,
        );
        GuardianDecision {
            operation_id: Some(OperationId::new("operation-install-repair")),
            mode: GuardianMode::Managed,
            kind: GuardianDecisionKind::Repair,
            diagnoses: vec![DiagnosisId::new("launcher_managed_artifact_corrupt")],
            action_plan: Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                    ownership,
                    confidence: GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![
                        GuardianActionKind::Quarantine,
                        GuardianActionKind::Repair,
                        GuardianActionKind::Block,
                    ],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                }],
            )),
        }
    }
}
