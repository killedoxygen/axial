//! Application system boundary.
//!
//! This module names the backend command orchestration and backend-authored
//! view model contracts. Routes adapt HTTP transport to these entrypoints while
//! remaining product workflow decisions move behind Application and owning
//! backend systems.

pub mod accounts;
pub mod auth;
pub mod commands;
pub mod config;
pub mod content;
pub mod flags;
mod guardian_conversion;
pub mod install;
pub mod instances;
mod integrity;
mod integrity_scheduler;
pub mod java;
mod known_good;
pub mod launch;
pub mod music;
pub mod performance;
mod persisted_state_repair;
mod registered_artifact_recovery;
pub mod setup;
pub mod skin;
pub mod status;
pub mod telemetry;
pub(crate) mod timing;
pub mod update;
pub mod version;
mod whole_instance_rematerialization;

use crate::guardian::{GuardianDecision, GuardianFact, SafetyOutcome};
use crate::observability::{EvidenceRecord, OperationEvent, PerformanceProofRecord};
use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationStatus, RollbackState,
    TargetDescriptor,
};
use serde::{Deserialize, Serialize};

pub(crate) use accounts::{
    AccountActionResponse, AccountListResponse, AccountPatchRequest, AccountRemoveResponse,
    OfflineAccountCreateRequest, accounts, create_offline_account, patch_account, remove_account,
    select_account, sync_active_offline_account_from_username,
};
pub(crate) use auth::{
    AuthRefreshFailure, AuthStatusResponse, auth_logout_for_state, auth_profile_sync_for_state,
    auth_refresh_for_state, auth_status, refresh_active_auth,
};
pub use commands::{
    ApplicationCommandPayload, ApplicationCommandRequest, ApplyPerformancePlanCommand,
    ApplyPerformancePlanPayload, CommandCatalogEntry, CommandPayloadStatus, CommandRequestContract,
    CommandResultCarrierKind, CommandResultContract, CommandSafetyReview, CreateInstanceCommand,
    CreateInstancePayload, InstallVersionCommand, InstallVersionPayload, LaunchInstanceCommand,
    LaunchInstancePayload, PerformancePlanCommandAction, RefreshAccountReadinessCommand,
    RefreshAccountReadinessPayload, RefreshPerformanceRulesCommand, RefreshPerformanceRulesPayload,
    RepairInstanceCommand, RepairInstancePayload, StopSessionCommand, StopSessionPayload,
    StopSessionReason, ValidateInstanceCommand, ValidateInstancePayload, command_catalog,
    phase_one_command_kinds,
};
pub use config::{ConfigPatch, current_config, update_config};
pub use content::pack::{modpack_files, modpack_target};
pub use content::{
    ContentApiError, ContentCompatRequest, ContentCompatResponse, ContentInstallRequest,
    ContentPlanRequest, ContentSearchParams, ContentUpdatesResponse, InstanceContentResponse,
    ModpackFilesPlan, ModpackInstallRequest, ModpackInstallResponse, ModpackTarget, ResolutionPlan,
    SearchHit, content_detail, content_plan, content_search, instance_content,
    instance_content_updates,
};
pub(crate) use content::{
    content_compatibility, pack::queue_modpack_install, queue_content_install,
    queue_content_uninstall, queue_content_uninstalls,
};
pub use flags::{
    FlagOverridePatch, FlagSource, FlagViewModel, FlagsResponse, list_flags, update_flag,
};
pub(crate) use install::enqueue_install_with_dependency_admitted;
pub use install::{
    InstallApplicationError, InstallProgressStepViewModel, InstallProgressViewModel,
    InstallQueueContentActionRequest, InstallQueueContentItemViewModel,
    InstallQueueContentSelection, InstallQueueRequest, InstallQueueStateResponse,
    InstallStartResponse, InstallStatusResponse, InstallVersionStaging, InstallVersionStartRequest,
    LoaderBuildsRequest, LoaderInstallStartRequest, begin_install_operation_journal,
    install_events_stream, install_operation_id, install_status, loader_builds, loader_components,
    loader_game_versions, loader_install_events_stream, loader_install_progress_view_model,
    loader_pre_operation_error_response, public_loader_install_progress_json,
    public_vanilla_install_progress_json, record_install_operation_interrupted,
    record_install_operation_progress, remove_queued_install, sanitize_install_progress,
    stage_install_version_command, vanilla_install_progress_view_model,
};
pub(crate) use install::{
    enqueue_install_from_continuation, enqueue_install_owned, install_queue_status_owned,
    retry_install_owned,
};
pub(crate) use integrity_scheduler::spawn_idle_integrity_scheduler;
pub use java::{JavaRuntimesResponse, java_runtimes};
pub(crate) use known_good::{
    rebuild_registered_known_good, registered_known_good_is_live, spawn_startup_known_good_rebuilds,
};
pub use launch::{
    LaunchInstanceStaging, LaunchPreflightMemory, LaunchPreflightOverride,
    LaunchPreflightOverrides, LaunchPreflightResourceBudget, LaunchPreflightResponse,
    LaunchRequest, LaunchRequestError, LaunchSuccess, launch_application_stage_evidence,
    launch_request_error_response_payload, launch_success_response_payload,
    prepare_launch_preflight, sanitize_live_launch_failure_message, stage_launch_instance_command,
    trace_launch_event,
};
pub(crate) use launch::{launch_notice_from_values, launch_preflight_stage_evidence};
pub use music::{
    MusicStatusResponse, MusicTrackBytes, MusicTrackError, MusicTrackRequest, music_status,
    music_track,
};
pub use performance::{
    PerformanceHealthRequest, PerformanceHealthResponse, PerformanceInstallRequest,
    PerformanceInstallResponse, PerformanceInstanceDisplay, PerformanceInstanceOperationResponse,
    PerformanceManagedArtifactSummary, PerformanceMemoryDisplay, PerformanceModeDisplay,
    PerformanceOperationStatusResponse, PerformancePlanRequest, PerformancePlanResponse,
    PerformanceRollbackListRequest, PerformanceRollbackListResponse,
    PerformanceRulesStatusResponse, PerformanceRuntimeDisplay, RefreshPerformanceRulesError,
    SystemResourceResponse, performance_instance_operation, performance_operation_status,
    performance_plan, performance_plan_summary_view_model, performance_rollback_list,
    performance_rules_status, refresh_performance_rules_error_response, system_resource_status,
};
pub(crate) use performance::{
    performance_health, performance_install, refresh_performance_rules,
    spawn_pending_performance_operations,
};
pub(crate) use persisted_state_repair::settle_startup_persisted_state_repairs;
pub(crate) use setup::setup_init_owned;
pub use setup::{SetupLibraryResponse, SetupStatusResponse, onboarding_complete};
pub(crate) use skin::flush_pending_saved_skin_applies_for_launch;
pub use skin::flush_pending_saved_skin_applies_for_shutdown;
pub use status::{StatusResponse, launcher_status};
pub use telemetry::{FrontendErrorReportRequest, report_frontend_error};
pub use update::{
    UpdateDownloadRequest, UpdateFlowResponse, UpdateResponse, update_flow_state, update_status,
};
pub(crate) use update::{apply_staged_update, cleanup_update_staging, start_update_download};
pub use version::{
    CatalogEntry, CatalogResponse, DeleteVersionRequest, SharedDataInfo, VersionInfoResponse,
    VersionsResponse, WorldInfo, open_version_folder,
};
pub(crate) use version::{
    catalog, delete_version, installed_versions, installed_versions_event_payload, version_info,
};
#[cfg(test)]
pub(crate) use whole_instance_rematerialization::spawn_explicit_whole_instance_rematerialization_with;
#[expect(
    unused_imports,
    reason = "Phase 4 backend contract; Phase 6 transport deferred"
)]
pub(crate) use whole_instance_rematerialization::{
    ExplicitWholeInstanceRematerializationError, execute_explicit_whole_instance_rematerialization,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApplicationOutcome<TPayload = EmptyPayload> {
    pub result: CommandResult<TPayload>,
    pub next_actions: Vec<CommandKind>,
    pub guardian_decision: Option<GuardianDecision>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EmptyPayload;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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
    pub effective_plan: Option<axial_performance::EffectivePerformancePlan>,
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
    PerformancePlanSummary,
    OperationProgress,
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
    PerformancePlanSummary(PerformancePlanSummaryViewModel),
    OperationProgress(OperationProgressViewModel),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
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
