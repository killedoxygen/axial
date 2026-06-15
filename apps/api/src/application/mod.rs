//! Application system boundary.
//!
//! This module names the backend command orchestration and backend-authored
//! view model contracts. Routes adapt HTTP transport to these entrypoints while
//! remaining product workflow decisions move behind Application and owning
//! backend systems.

pub mod accounts;
pub mod auth;
pub mod authority;
pub mod commands;
pub mod install;
pub mod instances;
pub mod launch;
pub mod performance;
pub mod skin;
pub mod update;
pub mod version;

use crate::guardian::{GuardianDecision, GuardianFact, SafetyOutcome};
use crate::observability::{EvidenceRecord, OperationEvent, PerformanceProofRecord};
use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationStatus, RollbackState,
    TargetDescriptor,
};
use serde::{Deserialize, Serialize};

pub(crate) use accounts::{
    AccountActionResponse, AccountListResponse, AccountPatchRequest, OfflineAccountCreateRequest,
    accounts, create_offline_account, patch_account, remove_account, select_account,
    sync_active_offline_account_from_username,
};
pub(crate) use auth::{
    AuthRefreshFailure, AuthStatusResponse, auth_logout_for_state, auth_profile_sync_for_state,
    auth_refresh_for_state, auth_status, refresh_active_auth,
};
pub use authority::{
    AuthorityCutLine, DecisionCategory, DecisionLocation, RouteAdapterContract,
    RouteAdapterResponsibility, RouteBoundaryEnforcement, RouteBoundaryProbe, RouteCutoverPhase,
    RouteForbiddenResponsibility, RouteHotspotOwner, RouteWorkflowArea, RouteWorkflowHotspot,
    authority_cut_lines, route_adapter_contract, route_boundary_probes, route_workflow_hotspots,
};
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
    InstallApplicationError, InstallGuardianRepairSummary, InstallStartResponse,
    InstallStatusResponse, InstallVersionStaging, InstallVersionStartRequest, LoaderBuildsRequest,
    LoaderInstallStartRequest, begin_install_operation_journal,
    install_guardian_repair_summary_from_journal, install_operation_id, install_status,
    loader_builds, loader_components, loader_error_response, loader_game_versions,
    record_install_operation_guardian_evidence, record_install_operation_guardian_repair_outcome,
    record_install_operation_interrupted, record_install_operation_progress,
    repair_install_artifact_corruption_with_guardian, sanitize_install_progress,
    stage_install_version_command, start_install_version, start_loader_install,
};
pub use launch::{
    LaunchBoundaryStaging, LaunchBoundaryStagingRequest, LaunchInstanceStaging,
    LaunchPreflightMemory, LaunchPreflightOverride, LaunchPreflightOverrides,
    LaunchPreflightResourceBudget, LaunchPreflightResponse, LaunchRequest, LaunchRequestError,
    LaunchSessionTask, LaunchSuccess, PreparedLaunch, launch_application_stage_evidence,
    launch_benchmark_status_payload, launch_boundary_stage_evidence,
    launch_prepared_response_payload, launch_request_error_response_payload, launch_session,
    launch_success_response_payload, persist_launch_proof_best_effort, prepare_launch_preflight,
    prepare_launch_session, sanitize_live_launch_failure_message, stage_launch_boundary,
    stage_launch_instance_command, trace_launch_event,
};
pub use performance::{
    PerformanceHealthRequest, PerformanceHealthResponse, PerformanceInstallRequest,
    PerformanceInstallResponse, PerformanceInstanceDisplay, PerformanceInstanceOperationResponse,
    PerformanceManagedArtifactSummary, PerformanceMemoryDisplay, PerformanceModeDisplay,
    PerformancePlanRequest, PerformancePlanResponse, PerformanceRollbackListRequest,
    PerformanceRollbackListResponse, PerformanceRulesStatusResponse, PerformanceRuntimeDisplay,
    RefreshPerformanceRulesError, performance_health, performance_install,
    performance_instance_operation, performance_operation_status, performance_plan,
    performance_plan_summary_view_model, performance_rollback_list, performance_rules_status,
    refresh_performance_rules, refresh_performance_rules_error_response,
    spawn_pending_performance_operations,
};
pub(crate) use skin::flush_pending_saved_skin_applies_for_launch;
pub use skin::flush_pending_saved_skin_applies_for_shutdown;
pub use update::{UpdateResponse, update_status};
pub use version::{
    CatalogEntry, CatalogResponse, DeleteVersionRequest, SharedDataInfo, VersionInfoResponse,
    VersionsResponse, WorldInfo, catalog, delete_version, installed_versions,
    installed_versions_event_payload, open_version_folder, version_info,
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
