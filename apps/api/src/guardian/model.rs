use crate::observability::EvidenceField;
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianMode {
    Managed,
    Custom,
    Disabled,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct GuardianFactId(pub String);

impl GuardianFactId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianFact {
    pub operation_id: Option<OperationId>,
    pub id: GuardianFactId,
    pub domain: GuardianDomain,
    pub phase: OperationPhase,
    pub reliability: FactReliability,
    pub severity: Option<GuardianSeverity>,
    pub confidence: Option<GuardianConfidence>,
    pub ownership: OwnershipClass,
    pub target: Option<TargetDescriptor>,
    pub fields: Vec<EvidenceField>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FactReliability {
    DirectStructured,
    ValidatedProbe,
    ProcessLifecycle,
    ExactClassifier,
    HeuristicClassifier,
    ExpectedMarkerAbsence,
    UserReported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosisId(pub String);

impl DiagnosisId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub id: DiagnosisId,
    pub domain: GuardianDomain,
    pub severity: GuardianSeverity,
    pub confidence: GuardianConfidence,
    pub ownership: OwnershipClass,
    pub phase: OperationPhase,
    pub fact_ids: Vec<String>,
    pub affected_targets: Vec<TargetDescriptor>,
    pub impact: GuardianImpactVector,
    pub candidate_actions: Vec<GuardianActionKind>,
    pub public_reason_template: String,
}

impl Diagnosis {
    pub fn action_prerequisite(&self) -> Result<ActionPlanPrerequisite, GuardianCoreError> {
        if self.affected_targets.is_empty() {
            return Err(GuardianCoreError::MissingAffectedTarget);
        }
        if self.candidate_actions.is_empty() {
            return Err(GuardianCoreError::MissingCandidateAction);
        }
        Ok(ActionPlanPrerequisite {
            diagnosis_id: self.id.clone(),
            ownership: self.ownership,
            confidence: self.confidence,
            affected_targets: self.affected_targets.clone(),
            candidate_actions: self.candidate_actions.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianDomain {
    Config,
    Library,
    Runtime,
    Jvm,
    Install,
    Download,
    Performance,
    Launch,
    Startup,
    Session,
    Filesystem,
    Network,
    Auth,
    State,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianSeverity {
    Info,
    Warning,
    Degraded,
    Repairable,
    Recoverable,
    Blocking,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianConfidence {
    Low,
    Medium,
    High,
    Confirmed,
    Certain,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GuardianImpactVector {
    pub privacy_risk: f32,
    pub data_loss_risk: f32,
    pub launchability_impact: f32,
    pub state_corruption_impact: f32,
    pub user_intent_impact: f32,
    pub performance_impact: f32,
    pub host_stability_impact: f32,
}

impl GuardianImpactVector {
    pub fn scalar_severity(self) -> f32 {
        self.privacy_risk
            .max(self.data_loss_risk)
            .max(self.launchability_impact * 0.90)
            .max(self.state_corruption_impact * 0.85)
            .max(self.user_intent_impact * 0.70)
            .max(self.host_stability_impact * 0.65)
            .max(self.performance_impact * 0.45)
    }

    pub(crate) fn launch_blocking() -> Self {
        Self {
            launchability_impact: 0.95,
            ..Self::default()
        }
    }

    pub(crate) fn repairable_corruption() -> Self {
        Self {
            launchability_impact: 0.80,
            state_corruption_impact: 0.85,
            ..Self::default()
        }
    }

    pub(crate) fn record_only() -> Self {
        Self {
            launchability_impact: 0.15,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SafetyCase {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub phase: OperationPhase,
    pub diagnoses: Vec<Diagnosis>,
    pub hard_constraints: Vec<GuardianHardConstraint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianHardConstraint {
    OwnershipRequired,
    RedactionRequired,
    JournalRequiredForMutation,
    UserOwnedDestructiveMutationForbidden,
    UnknownOwnedDestructiveMutationForbidden,
    RetryLoopForbidden,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionPlanPrerequisite {
    pub diagnosis_id: DiagnosisId,
    pub ownership: OwnershipClass,
    pub confidence: GuardianConfidence,
    pub affected_targets: Vec<TargetDescriptor>,
    pub candidate_actions: Vec<GuardianActionKind>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianDecision {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub kind: GuardianDecisionKind,
    pub diagnoses: Vec<DiagnosisId>,
    pub action_plan: Option<GuardianActionPlan>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianDecisionKind {
    Allow,
    Warn,
    Repair,
    Retry,
    Replace,
    Strip,
    Downgrade,
    Degrade,
    Fallback,
    Quarantine,
    Rollback,
    Block,
    AskUser,
    RecordOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianActionPlan {
    pub owner: StabilizationSystem,
    pub prerequisite: ActionPlanPrerequisite,
    pub actions: Vec<GuardianAction>,
}

impl GuardianActionPlan {
    pub fn new(
        owner: StabilizationSystem,
        prerequisite: ActionPlanPrerequisite,
        actions: Vec<GuardianAction>,
    ) -> Self {
        Self {
            owner,
            prerequisite,
            actions,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianAction {
    pub kind: GuardianActionKind,
    pub target: Option<TargetDescriptor>,
    pub reason: DiagnosisId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianActionKind {
    Allow,
    Warn,
    Repair,
    Retry,
    Replace,
    Strip,
    Downgrade,
    Degrade,
    Fallback,
    Quarantine,
    Rollback,
    AskUser,
    Block,
    RecordOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SafetyOutcome {
    pub decision: GuardianDecisionKind,
    pub summary: String,
    pub detail: Option<String>,
    pub diagnoses: Vec<DiagnosisId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCoreError {
    MissingAffectedTarget,
    MissingCandidateAction,
}
