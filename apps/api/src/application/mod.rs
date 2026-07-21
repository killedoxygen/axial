//! Application system boundary.
//!
//! Routes adapt HTTP transport to backend-owned workflow entrypoints while
//! product decisions remain behind Application and their owning systems.

pub mod accounts;
pub mod auth;
pub mod config;
pub mod content;
mod filesystem;
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

pub(crate) use accounts::{
    AccountActionResponse, AccountListResponse, AccountPatchRequest, AccountRemoveResponse,
    OfflineAccountCreateRequest, accounts, create_offline_account, patch_account, remove_account,
    select_account, sync_active_offline_account_from_username,
};
pub(crate) use auth::{
    AuthRefreshFailure, AuthStatusResponse, auth_logout_for_state, auth_profile_sync_for_state,
    auth_refresh_for_state, auth_status, refresh_active_auth,
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
    InstallApplicationError, InstallProgressJournalTracker, InstallProgressStepViewModel,
    InstallProgressViewModel, InstallQueueContentActionRequest, InstallQueueContentItemViewModel,
    InstallQueueContentSelection, InstallQueueRequest, InstallQueueStateResponse,
    InstallStartResponse, InstallStatusResponse, InstallVersionStartRequest, LoaderBuildsRequest,
    LoaderInstallStartRequest, begin_install_operation_journal, install_operation_id,
    install_status, loader_builds, loader_components, loader_game_versions,
    loader_pre_operation_error_response, public_loader_install_progress_record_json,
    public_vanilla_install_progress_record_json, record_install_operation_interrupted,
    record_install_operation_progress, sanitize_install_progress,
};
pub(crate) use install::{
    enqueue_install_from_continuation, enqueue_install_owned, install_events_stream,
    install_queue_status_owned, loader_install_events_stream, remove_queued_install_owned,
    retry_install_owned, settle_startup_install_guardian_failure_memory,
};
pub(crate) use integrity_scheduler::spawn_idle_integrity_scheduler;
pub use java::{JavaRuntimesResponse, java_runtimes};
pub(crate) use known_good::{
    rebuild_registered_known_good, registered_known_good_is_live, spawn_startup_known_good_rebuilds,
};
pub(crate) use launch::launch_preflight_stage_evidence;
pub use launch::{
    LaunchPreflightMemory, LaunchPreflightOverride, LaunchPreflightOverrides,
    LaunchPreflightResourceBudget, LaunchPreflightResponse, LaunchRequest,
    prepare_launch_preflight,
};
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
