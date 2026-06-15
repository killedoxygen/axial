//! Application system boundary.
//!
//! This module names the backend command orchestration and backend-authored
//! view model contracts. Current routes still execute through their existing
//! paths until later cutover phases move workflow behavior behind these types.

pub mod authority;
pub mod commands;
pub mod install;
pub mod launch;
pub mod performance;

use crate::guardian::{GuardianDecision, GuardianFact, SafetyOutcome};
use crate::observability::{EvidenceRecord, OperationEvent, PerformanceProofRecord};
use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationStatus, RollbackState,
    TargetDescriptor,
};
use serde::{Deserialize, Serialize};

pub use authority::{AuthorityCutLine, DecisionCategory, DecisionLocation, authority_cut_lines};
pub use commands::{
    ApplicationCommandPayload, ApplicationCommandRequest, ApplyPerformancePlanCommand,
    ApplyPerformancePlanPayload, CommandCatalogEntry, CommandPayloadStatus, CommandRequestContract,
    CommandResultCarrierKind, CommandResultContract, CommandSafetyReview, InstallVersionCommand,
    InstallVersionPayload, LaunchInstanceCommand, LaunchInstancePayload,
    PerformancePlanCommandAction, RefreshAccountReadinessCommand, RefreshAccountReadinessPayload,
    RefreshPerformanceRulesCommand, RefreshPerformanceRulesPayload, RepairInstanceCommand,
    RepairInstancePayload, StopSessionCommand, StopSessionPayload, StopSessionReason,
    ValidateInstanceCommand, ValidateInstancePayload, command_catalog, phase_one_command_kinds,
};
pub use install::{
    InstallGuardianRepairSummary, InstallVersionStaging, begin_install_operation_journal,
    install_guardian_repair_summary_from_journal, install_operation_id,
    record_install_operation_guardian_evidence, record_install_operation_guardian_repair_outcome,
    record_install_operation_interrupted, record_install_operation_progress,
    repair_install_artifact_corruption_with_guardian, stage_install_version_command,
};
pub use launch::{
    LaunchBoundaryStaging, LaunchBoundaryStagingRequest, LaunchInstanceStaging,
    launch_application_stage_evidence, launch_boundary_stage_evidence, stage_launch_boundary,
    stage_launch_instance_command,
};
pub use performance::{
    PerformanceRulesStatusResponse, RefreshPerformanceRulesError,
    performance_plan_summary_view_model, performance_rules_status, refresh_performance_rules,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplicationCommand {
    pub kind: CommandKind,
    pub target: Option<TargetDescriptor>,
    pub requested_operation: Option<OperationId>,
}

impl ApplicationCommand {
    pub fn new(kind: CommandKind) -> Self {
        Self {
            kind,
            target: None,
            requested_operation: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandResult<TPayload = EmptyPayload> {
    pub command: CommandKind,
    pub operation_id: Option<OperationId>,
    pub status: OperationStatus,
    pub safety: Option<SafetyOutcome>,
    #[serde(default, skip_serializing_if = "CommandResultCarriers::is_empty")]
    pub carriers: CommandResultCarriers,
    pub payload: TPayload,
    pub view_model: Option<ApplicationViewModel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplicationOutcome<TPayload = EmptyPayload> {
    pub result: CommandResult<TPayload>,
    pub next_actions: Vec<CommandKind>,
    pub guardian_decision: Option<GuardianDecision>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EmptyPayload;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandResultCarriers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian: Option<GuardianCommandCarrier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub performance: Option<PerformanceCommandCarrier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationCommandCarrier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionCommandCarrier>,
}

impl CommandResultCarriers {
    pub fn is_empty(&self) -> bool {
        self.guardian.is_none()
            && self.performance.is_none()
            && self.operation.is_none()
            && self.session.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianCommandCarrier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<GuardianDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety: Option<SafetyOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<GuardianFact>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerformanceCommandCarrier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_plan: Option<croopor_performance::EffectivePerformancePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<PerformanceProofRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationCommandCarrier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<OperationStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<OperationJournalEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<OperationEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRecord>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionCommandCarrier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplicationViewModel {
    pub kind: ViewModelKind,
    pub target: Option<TargetDescriptor>,
    pub notices: Vec<BackendNotice>,
    pub available_actions: Vec<CommandKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ApplicationViewModelPayload>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ViewModelKind {
    InstanceDetail,
    LaunchActionState,
    GuardianSafetyState,
    PerformancePlanSummary,
    OperationProgress,
    RepairOffer,
    SessionOutcome,
    SettingsState,
    AccountReadiness,
    AccountReadinessState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum ApplicationViewModelPayload {
    InstanceDetail(InstanceDetailViewModel),
    LaunchActionState(LaunchActionStateViewModel),
    GuardianSafetyState(GuardianSafetyStateViewModel),
    PerformancePlanSummary(PerformancePlanSummaryViewModel),
    OperationProgress(OperationProgressViewModel),
    RepairOffer(RepairOfferViewModel),
    SessionOutcome(SessionOutcomeViewModel),
    SettingsState(SettingsStateViewModel),
    AccountReadiness(AccountReadinessViewModel),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewModelTone {
    Ok,
    Warn,
    Err,
    Mute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewModelAction {
    pub command: CommandKind,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstanceDetailViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchActionStateViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    pub launchable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianSafetyStateViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub diagnosis_ids: Vec<String>,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerformancePlanSummaryViewModel {
    pub state_id: String,
    pub title: String,
    pub detail: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition_id: Option<String>,
    #[serde(default)]
    pub managed_artifact_count: usize,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationProgressViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<OperationStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepairOfferViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    pub available: bool,
    pub requires_confirmation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionOutcomeViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsStateViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountReadinessViewModel {
    pub state_id: String,
    pub label: String,
    pub tone: ViewModelTone,
    pub online_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendNotice {
    pub level: NoticeLevel,
    pub message: String,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}
