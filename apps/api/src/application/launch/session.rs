mod auth;
mod overrides;
mod readiness;
mod resources;
mod runtime_repair;

#[cfg(test)]
pub(crate) use readiness::readiness_guardian_facts_for_coverage;

use super::policy;
use super::runner::trace_launch_event;
use crate::application::guardian_conversion::api_guardian_mode;
use crate::application::timing::{
    LaunchPreflightFactTiming, LaunchPreflightResponseTiming, LaunchPreflightSenseTimings,
    LaunchSessionTiming, trace_launch_preflight_facts, trace_launch_preflight_response,
    trace_launch_session,
};
use crate::application::version::VERSION_SCAN_DEGRADED_MESSAGE;
use crate::application::{
    LaunchInstanceCommand, LaunchInstanceStaging, flush_pending_saved_skin_applies_for_launch,
    launch_preflight_stage_evidence, stage_launch_instance_command,
};
use crate::execution::integrity::{IntegrityTier0Report, sense_integrity_tier0};
use crate::guardian::{
    GuardianActionKind as ApiGuardianActionKind, GuardianDirective, GuardianFact,
    GuardianLaunchAdmission, GuardianLaunchFailureMemoryIntakeRequest,
    GuardianLaunchRecoveryCurrentIntent, GuardianManagedJavaReason, GuardianPreflightOutcome,
    GuardianPreflightOutcomeRequest, GuardianPreflightReadiness, GuardianStripJvmArgsReason,
    GuardianSummary, guardian_fact_from_execution, guardian_preflight_outcome,
    guardian_summary_from_admission, launch_failure_memory_guardian_facts, launch_notice,
};
use crate::logging::timestamp_utc;
use crate::state::contracts::OperationPhase;
use crate::state::launch_reports::{LaunchBenchmarkMetadata, LaunchProofResourceBudget};
use crate::state::{
    AppState, InstanceLifecycleLease, IntegrityForegroundLease, LaunchSessionRecord,
    UpdateOperationAdmissionError, UpdateOperationLease,
};
use auth::{LaunchAuthRefreshOptions, resolve_launch_auth_context};
use axial_config::{AppConfig, Instance};
use axial_launcher::{
    GuardianMode, LaunchGuardianContext, LaunchIntent, LaunchReadiness, LaunchReadinessReason,
    LaunchReadinessReasonId, LaunchReadinessRequest, LaunchReadinessSeverity, LaunchStageEvidence,
    LaunchState, inspect_launch_readiness_structural,
};
use axial_minecraft::known_good::LaunchTier0RuntimeSelection;
use axial_minecraft::{
    JavaRuntimeProbeReceipt, RuntimeOverride, VersionScanState, parse_runtime_override,
};
use axum::{Json, http::StatusCode};
use overrides::{
    inspect_explicit_java_override, inspect_explicit_jvm_args, preflight_override_signals,
};
#[cfg(test)]
use readiness::readiness_has_managed_runtime_missing;
use readiness::{append_integrity_readiness_reasons, readiness_guardian_facts};
use resources::{
    ActiveLaunchResourceUse, LaunchMemoryEvidence, capture_launch_cpu_load_evidence,
    capture_launch_disk_evidence, capture_launch_memory_evidence, capture_resource_budget_snapshot,
    host_cpu_threads, preflight_resource_signals,
};
#[cfg(test)]
use resources::{LaunchCpuLoadEvidence, LaunchDiskEvidence, load_to_x100};
#[cfg(test)]
use runtime_repair::maybe_repair_managed_runtime_before_launch_with_fixture;
use runtime_repair::{
    ManagedRuntimeRepairLaunch, RuntimeComponentRebuildSource,
    maybe_repair_managed_runtime_before_launch_owned,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Debug, Deserialize)]
pub struct LaunchRequest {
    pub instance_id: String,
    pub username: Option<String>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub client_started_at_ms: Option<i64>,
}

pub(crate) struct LaunchSessionTask {
    pub update_admission: UpdateOperationLease,
    pub integrity_foreground: IntegrityForegroundLease,
    pub application: LaunchInstanceStaging,
    pub preflight_stage_evidence: Vec<LaunchStageEvidence>,
    pub instance: Instance,
    pub intent: LaunchIntent,
    pub guardian: GuardianSummary,
    pub launched_at: String,
    pub benchmark: Option<LaunchBenchmarkMetadata>,
    pub resource_budget: Option<LaunchProofResourceBudget>,
    pub java_probe_receipt: Option<JavaRuntimeProbeReceipt>,
}

pub(crate) struct PreparedLaunch {
    pub task: LaunchSessionTask,
}

struct LaunchPreflightFacts {
    config: AppConfig,
    max_memory_mb: i32,
    raw_min_memory_mb: i32,
    min_memory_mb: i32,
    requested_java: String,
    requested_preset: String,
    extra_jvm_args: Vec<String>,
    target_version_id: String,
    loader: String,
    is_modded: bool,
    guardian: LaunchGuardianContext,
    guardian_summary: GuardianSummary,
    guardian_outcome: GuardianPreflightOutcome,
    guardian_admission: GuardianLaunchAdmission,
    guardian_facts: Vec<GuardianFact>,
    preflight_stage_evidence: Vec<LaunchStageEvidence>,
    readiness: LaunchReadiness,
    resource_budget: LaunchProofResourceBudget,
    java_probe_receipt: Option<JavaRuntimeProbeReceipt>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflightResponse {
    pub status: &'static str,
    pub guardian: GuardianSummary,
    pub mode: GuardianMode,
    pub memory: LaunchPreflightMemory,
    pub overrides: LaunchPreflightOverrides,
    pub readiness: LaunchReadiness,
    pub guardian_facts: Vec<GuardianFact>,
    pub resource_budget: LaunchPreflightResourceBudget,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflightMemory {
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub min_clamped: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflightOverrides {
    pub java: LaunchPreflightOverride,
    pub preset: LaunchPreflightOverride,
    pub raw_jvm_args: LaunchPreflightOverride,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflightOverride {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<axial_launcher::OverrideOrigin>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflightResourceBudget {
    pub active_session_count: usize,
    pub active_install_count: usize,
    pub active_memory_allocation_mb: u64,
    pub requested_memory_mb: Option<i32>,
    pub estimated_remaining_memory_mb: Option<i64>,
    pub memory_pressure: bool,
    pub cpu_pressure: bool,
    pub install_pressure: bool,
    pub disk_pressure: bool,
}

pub(crate) async fn prepare_launch_session_owned(
    state: &AppState,
    payload: LaunchRequest,
    producer: &crate::state::ProducerLease,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    prepare_launch_session_with_auth_refresh(
        state,
        payload,
        None,
        producer,
        RuntimeComponentRebuildSource::Production,
    )
    .await
}

#[cfg(test)]
pub(super) async fn prepare_launch_session_owned_with_runtime_fixture(
    state: &AppState,
    payload: LaunchRequest,
    producer: &crate::state::ProducerLease,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    prepare_launch_session_with_auth_refresh(
        state,
        payload,
        None,
        producer,
        RuntimeComponentRebuildSource::Fixture,
    )
    .await
}

#[cfg(test)]
pub(super) async fn prepare_launch_session(
    state: &AppState,
    payload: LaunchRequest,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    let producer = state
        .try_claim_producer()
        .map_err(super::launch_shutdown_error_response)?;
    prepare_launch_session_owned(state, payload, &producer).await
}

async fn prepare_launch_session_with_auth_refresh(
    state: &AppState,
    payload: LaunchRequest,
    auth_refresh: Option<LaunchAuthRefreshOptions>,
    producer: &crate::state::ProducerLease,
    runtime_rebuild_source: RuntimeComponentRebuildSource,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    let started_at = Instant::now();
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(launch_update_admission_error_response)?;
    let integrity_foreground = state
        .register_integrity_foreground()
        .map_err(launch_integrity_admission_error_response)?
        .wait_for_settlement()
        .await;
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Axial library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);
    let instance_lifecycle = state
        .try_acquire_integrity_instance_lifecycle(&integrity_foreground, &payload.instance_id)
        .await
        .map_err(launch_integrity_ownership_error_response)?
        .ok_or_else(launch_instance_busy_error_response)?;

    let instance = state.instances().get(&payload.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "instance not found" })),
        )
    })?;
    if state.sessions().has_active_instance(&instance.id).await {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "instance already has an active session" })),
        ));
    }
    let game_dir = state
        .instances()
        .registered_game_dir(&instance)
        .await
        .map_err(launch_instance_root_error_response)?;

    let config = state.config().current();
    let auth_started_at = Instant::now();
    let auth_context =
        resolve_launch_auth_context(state, &config, payload.username.as_deref(), auth_refresh)
            .await?;
    let auth_elapsed = auth_started_at.elapsed();
    if auth_context.online_launch {
        flush_pending_saved_skin_applies_for_launch(state).await?;
    }
    let username = auth_context.username.clone();
    let preflight_started_at = Instant::now();
    let mut preflight = build_launch_preflight_facts(
        state,
        producer,
        LaunchPreflightBuild {
            integrity_foreground: &integrity_foreground,
            instance_lifecycle: &instance_lifecycle,
            instance: &instance,
            config: &config,
            library_dir: &library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: payload.max_memory_mb,
            requested_min_memory_mb: payload.min_memory_mb,
        },
        None,
    )
    .await;
    let preflight_elapsed = preflight_started_at.elapsed();
    let repair_started_at = Instant::now();
    let repair_launch = ManagedRuntimeRepairLaunch {
        instance_lifecycle: &instance_lifecycle,
        instance: &instance,
        library_dir: &library_dir,
        game_dir: &game_dir,
        requested_max_memory_mb: payload.max_memory_mb,
        requested_min_memory_mb: payload.min_memory_mb,
    };
    preflight = match runtime_rebuild_source {
        RuntimeComponentRebuildSource::Production => {
            maybe_repair_managed_runtime_before_launch_owned(
                state,
                producer,
                &integrity_foreground,
                preflight,
                repair_launch,
            )
            .await
        }
        #[cfg(test)]
        RuntimeComponentRebuildSource::Fixture => {
            maybe_repair_managed_runtime_before_launch_with_fixture(
                state,
                producer,
                &integrity_foreground,
                preflight,
                repair_launch,
            )
            .await
        }
    }
    .map_err(launch_journal_error_response)?;
    let repair_elapsed = repair_started_at.elapsed();
    if preflight.guardian_admission.user_outcome().decision() == ApiGuardianActionKind::Block {
        trace_launch_session(
            LaunchSessionTiming {
                route: "/api/v1/launch",
                session_id: None,
                instance_id: &instance.id,
                version_id: &instance.version_id,
                total: started_at.elapsed(),
                auth: auth_elapsed,
                preflight: preflight_elapsed,
                runtime_repair: repair_elapsed,
                insert: None,
                readiness_launchable: preflight.readiness.launchable,
                guardian_decision: preflight.guardian_admission.user_outcome().decision(),
            },
            "launch session blocked by preflight timing",
        );
        return Err(launch_preflight_guardian_error_response(
            preflight.readiness,
            preflight.guardian_summary,
            preflight.guardian_admission,
        ));
    }

    let launched_at = timestamp_utc();
    let session_id = policy::generate_session_id();
    let application = stage_launch_instance_command(
        LaunchInstanceCommand {
            instance_id: instance.id.clone(),
            username: payload.username.clone(),
            max_memory_mb: payload.max_memory_mb,
            min_memory_mb: payload.min_memory_mb,
            client_started_at_ms: payload.client_started_at_ms,
        },
        Some(session_id.0.clone()),
    );

    let intent = LaunchIntent {
        session_id: session_id.0.clone(),
        library_dir: library_dir.clone(),
        instance_id: instance.id.clone(),
        version_id: instance.version_id.clone(),
        target_version_id: preflight.target_version_id.clone(),
        loader: preflight.loader.clone(),
        is_modded: preflight.is_modded,
        username: username.clone(),
        auth: auth_context.auth,
        requested_java: preflight.requested_java.clone(),
        requested_preset: preflight.requested_preset.clone(),
        extra_jvm_args: preflight.extra_jvm_args.clone(),
        max_memory_mb: preflight.max_memory_mb,
        min_memory_mb: preflight.min_memory_mb,
        resolution: policy::selected_resolution(&instance, &config),
        launcher_name: "axial".to_string(),
        launcher_version: state.version().to_string(),
        game_dir: Some(game_dir),
        guardian: preflight.guardian.clone(),
        performance_mode: policy::selected_performance_mode(&instance, &config),
    };

    let insert_started_at = Instant::now();
    state
        .sessions()
        .insert(LaunchSessionRecord {
            session_id: session_id.clone(),
            instance_id: instance.id.clone(),
            version_id: instance.version_id.clone(),
            launched_at: Some(launched_at.clone()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            crash_evidence: None,
            healing: None,
            guardian: serde_json::to_value(&preflight.guardian_summary).ok(),
            outcome: None,
            stages: Vec::new(),
        })
        .await
        .map_err(launch_session_admission_error_response)?;
    drop(instance_lifecycle);
    let insert_elapsed = insert_started_at.elapsed();
    trace_launch_event(
        &session_id.0,
        &format!(
            "launch requested for instance {} version {} client_started_at_ms={:?}",
            instance.id, instance.version_id, payload.client_started_at_ms
        ),
    );
    trace_launch_session(
        LaunchSessionTiming {
            route: "/api/v1/launch",
            session_id: Some(&session_id.0),
            instance_id: &instance.id,
            version_id: &instance.version_id,
            total: started_at.elapsed(),
            auth: auth_elapsed,
            preflight: preflight_elapsed,
            runtime_repair: repair_elapsed,
            insert: Some(insert_elapsed),
            readiness_launchable: preflight.readiness.launchable,
            guardian_decision: preflight.guardian_admission.user_outcome().decision(),
        },
        "launch session preparation timing",
    );

    Ok(PreparedLaunch {
        task: LaunchSessionTask {
            update_admission,
            integrity_foreground,
            application,
            preflight_stage_evidence: preflight.preflight_stage_evidence,
            instance: instance.clone(),
            intent,
            guardian: preflight.guardian_summary,
            launched_at,
            benchmark: None,
            resource_budget: Some(preflight.resource_budget),
            java_probe_receipt: preflight.java_probe_receipt,
        },
    })
}

fn launch_update_admission_error_response(
    error: UpdateOperationAdmissionError,
) -> (StatusCode, Json<serde_json::Value>) {
    let message = match error {
        UpdateOperationAdmissionError::ApplyInProgress => {
            "Launches are unavailable while an update is being applied."
        }
        UpdateOperationAdmissionError::RestartPending => {
            "Restart Axial to finish the applied update before launching."
        }
    };
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": message })),
    )
}

fn launch_instance_root_error_response(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "Could not access the registered instance folder. Check app data permissions and try again."
        })),
    )
}

fn launch_journal_error_response(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "Could not record the launch repair safely. Check app data permissions and try again."
        })),
    )
}

fn launch_session_admission_error_response(
    _error: crate::state::SessionAdmissionError,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "Launches are unavailable while the application is shutting down."
        })),
    )
}

fn launch_integrity_admission_error_response(
    _error: crate::state::IntegrityActivityClosed,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "application shutdown is in progress" })),
    )
}

fn launch_integrity_ownership_error_response(
    _error: crate::state::IntegrityForegroundOwnershipError,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "launch integrity ownership could not be validated" })),
    )
}

fn launch_instance_busy_error_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "instance is busy with another launch or content operation"
        })),
    )
}

fn launch_preflight_guardian_error_response(
    readiness: LaunchReadiness,
    guardian: GuardianSummary,
    admission: GuardianLaunchAdmission,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = if readiness.launchable {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::PRECONDITION_FAILED
    };
    (
        status,
        Json(json!({
            "error": admission.user_outcome().summary(),
            "readiness": readiness,
            "notice": launch_notice(Some(&guardian), None, None, None, None),
            "guardian": guardian,
            "safety": admission.safety(),
        })),
    )
}

pub async fn prepare_launch_preflight(
    state: &AppState,
    instance_id: String,
) -> Result<LaunchPreflightResponse, (StatusCode, Json<serde_json::Value>)> {
    prepare_launch_preflight_with_memory_capture(state, instance_id, capture_launch_memory_evidence)
        .await
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LaunchPreflightMemoryProfile {
    pub(super) host_total_memory_mb: Option<u64>,
    pub(super) host_available_memory_mb: Option<u64>,
    pub(super) host_used_memory_mb: Option<u64>,
    pub(super) launcher_process_memory_mb: Option<u64>,
}

#[cfg(test)]
impl LaunchPreflightMemoryProfile {
    fn into_evidence(self) -> LaunchMemoryEvidence {
        LaunchMemoryEvidence {
            host_total_memory_mb: self.host_total_memory_mb,
            host_available_memory_mb: self.host_available_memory_mb,
            host_used_memory_mb: self.host_used_memory_mb,
            launcher_process_memory_mb: self.launcher_process_memory_mb,
        }
    }
}

#[cfg(test)]
pub(super) async fn prepare_launch_preflight_with_memory_profile_for_test(
    state: &AppState,
    instance_id: String,
    profile: LaunchPreflightMemoryProfile,
) -> Result<LaunchPreflightResponse, (StatusCode, Json<serde_json::Value>)> {
    prepare_launch_preflight_with_memory_capture(state, instance_id, move || {
        profile.into_evidence()
    })
    .await
}

async fn prepare_launch_preflight_with_memory_capture(
    state: &AppState,
    instance_id: String,
    capture_memory: impl FnOnce() -> LaunchMemoryEvidence,
) -> Result<LaunchPreflightResponse, (StatusCode, Json<serde_json::Value>)> {
    let started_at = Instant::now();
    let producer = state
        .try_claim_producer()
        .map_err(super::launch_shutdown_error_response)?;
    let integrity_foreground = state
        .register_integrity_foreground()
        .map_err(launch_integrity_admission_error_response)?
        .wait_for_settlement()
        .await;
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Axial library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);
    let instance_lifecycle = state
        .acquire_integrity_instance_lifecycle(&integrity_foreground, &instance_id)
        .await
        .map_err(launch_integrity_ownership_error_response)?;

    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "instance not found" })),
        )
    })?;
    let game_dir = state.instances().game_dir(&instance.id);
    let config = state.config().current();
    let facts = build_launch_preflight_facts_with_memory_capture(
        state,
        &producer,
        LaunchPreflightBuild {
            integrity_foreground: &integrity_foreground,
            instance_lifecycle: &instance_lifecycle,
            instance: &instance,
            config: &config,
            library_dir: &library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        None,
        capture_memory,
    )
    .await;

    trace_launch_preflight_response(LaunchPreflightResponseTiming {
        instance_id: &instance.id,
        version_id: &instance.version_id,
        total: started_at.elapsed(),
        readiness_launchable: facts.readiness.launchable,
        guardian_decision: facts.guardian_admission.user_outcome().decision(),
        reason_count: facts.readiness.reasons.len(),
        fact_count: facts.guardian_facts.len(),
    });

    Ok(facts.into_response())
}

struct LaunchPreflightBuild<'a> {
    integrity_foreground: &'a IntegrityForegroundLease,
    instance_lifecycle: &'a InstanceLifecycleLease,
    instance: &'a Instance,
    config: &'a AppConfig,
    library_dir: &'a Path,
    game_dir: &'a Path,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
}

async fn build_launch_preflight_facts(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    request: LaunchPreflightBuild<'_>,
    prior_java_probe_receipt: Option<JavaRuntimeProbeReceipt>,
) -> LaunchPreflightFacts {
    build_launch_preflight_facts_with_memory_capture(
        state,
        producer,
        request,
        prior_java_probe_receipt,
        capture_launch_memory_evidence,
    )
    .await
}

async fn build_launch_preflight_facts_with_memory_capture(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    request: LaunchPreflightBuild<'_>,
    prior_java_probe_receipt: Option<JavaRuntimeProbeReceipt>,
    capture_memory: impl FnOnce() -> LaunchMemoryEvidence,
) -> LaunchPreflightFacts {
    let LaunchPreflightBuild {
        integrity_foreground,
        instance_lifecycle,
        instance,
        config,
        library_dir,
        game_dir,
        requested_max_memory_mb,
        requested_min_memory_mb,
    } = request;
    let started_at = Instant::now();
    // Preflight is read-only: no session creation, installs, or raw path exposure.
    let memory_started_at = Instant::now();
    let memory_evidence = capture_memory();
    let memory_elapsed = memory_started_at.elapsed();
    let installed_versions_started_at = Instant::now();
    let installed_versions = state.installed_versions_snapshot(producer).await;
    let installed_versions_elapsed = installed_versions_started_at.elapsed();
    let report_matches_launch_root = installed_versions
        .as_ref()
        .is_some_and(|lookup| lookup.library_dir() == library_dir);
    let scan_source = if report_matches_launch_root {
        installed_versions
            .as_ref()
            .map(|lookup| lookup.source.as_str())
            .unwrap_or("unavailable")
    } else {
        "unavailable"
    };
    let refresh_count = installed_versions
        .as_ref()
        .map(|lookup| lookup.refresh_count)
        .unwrap_or_default();
    let version_report = installed_versions
        .as_ref()
        .filter(|_| report_matches_launch_root)
        .map(|lookup| lookup.snapshot.report());
    let version_scan_degraded =
        version_report.is_none_or(|report| report.state == VersionScanState::Degraded);
    let version_records = version_report
        .map(|report| report.versions.as_slice())
        .unwrap_or_default();
    let version_record = version_records
        .iter()
        .find(|version| version.id == instance.version_id);
    let target_version_id = version_record
        .and_then(|version| {
            let parent = version.inherits_from.trim();
            (!parent.is_empty()).then(|| parent.to_string())
        })
        .unwrap_or_else(|| instance.version_id.clone());
    let loader = version_record
        .and_then(|version| version.loader.as_ref())
        .map(|loader| loader.component_id.short_key().to_string())
        .unwrap_or_else(|| "vanilla".to_string());
    let is_modded = version_record.is_some_and(|version| {
        version.loader.is_some() || !version.inherits_from.trim().is_empty()
    });
    let memory_defaults = policy::derived_launch_memory_defaults(
        instance,
        config,
        version_record,
        requested_max_memory_mb,
        requested_min_memory_mb,
        memory_evidence.host_total_memory_mb,
    );
    let max_memory_mb =
        policy::effective_max_memory(instance, config, requested_max_memory_mb, memory_defaults);
    let raw_min_memory_mb =
        policy::selected_raw_min_memory(instance, config, requested_min_memory_mb, memory_defaults);
    let min_memory_mb = policy::effective_min_memory(
        instance,
        config,
        requested_min_memory_mb,
        max_memory_mb,
        memory_defaults,
    );
    let mut requested_java = policy::selected_java_override(instance, config);
    let requested_preset = policy::selected_jvm_preset(instance, config);
    let guardian = LaunchGuardianContext {
        mode: policy::selected_guardian_mode(config),
        java_override_origin: policy::java_override_origin(instance, config),
        preset_override_origin: policy::preset_override_origin(instance, config),
        raw_jvm_args_origin: policy::raw_jvm_args_origin(instance),
    };
    let required_java_major = version_record
        .and_then(|version| (version.java_major > 0).then_some(version.java_major as u32));
    let overrides_started_at = Instant::now();
    let java_inspection = inspect_explicit_java_override(
        state,
        producer,
        integrity_foreground,
        instance,
        config,
        required_java_major,
        prior_java_probe_receipt,
    )
    .await;
    let (mut execution_facts, mut java_probe_receipt, java_probe_count, java_probe_source) =
        java_inspection.map_or_else(
            || {
                (
                    Vec::new(),
                    None,
                    0,
                    overrides::PreflightJavaProbeSource::None,
                )
            },
            |inspection| {
                (
                    inspection.facts,
                    inspection.receipt,
                    inspection.probe_count,
                    inspection.probe_source,
                )
            },
        );
    let jvm_args_inspection = inspect_explicit_jvm_args(&instance.extra_jvm_args);
    let mut extra_jvm_args = jvm_args_inspection.args;
    execution_facts.extend(jvm_args_inspection.facts.iter().cloned());
    let overrides_elapsed = overrides_started_at.elapsed();
    let integrity_started_at = Instant::now();
    let requested_runtime = parse_runtime_override(&requested_java);
    let runtime_selection = if guardian.mode == GuardianMode::Custom {
        match &requested_runtime {
            RuntimeOverride::Component(component) => {
                LaunchTier0RuntimeSelection::ManagedComponent(component.0.as_str())
            }
            RuntimeOverride::ExecutablePath(_) => LaunchTier0RuntimeSelection::ExternalExecutable,
            RuntimeOverride::None => LaunchTier0RuntimeSelection::PreferredManaged,
        }
    } else {
        LaunchTier0RuntimeSelection::PreferredManaged
    };
    let (integrity_report, integrity_authority_unavailable) = match sense_integrity_tier0(
        state,
        integrity_foreground,
        instance_lifecycle,
        library_dir,
        runtime_selection,
    ) {
        Ok(report) => (report, false),
        Err(_) => (IntegrityTier0Report::default(), true),
    };
    let integrity_elapsed = integrity_started_at.elapsed();
    execution_facts.extend(integrity_report.facts.iter().cloned());
    let mut guardian_facts = execution_facts
        .iter()
        .map(|fact| guardian_fact_from_execution(fact, OperationPhase::Validating))
        .collect::<Vec<_>>();
    let performance_mode = policy::selected_performance_mode(instance, config);
    let resources_started_at = Instant::now();
    let resource_budget = capture_resource_budget_snapshot(
        memory_evidence,
        capture_launch_disk_evidence([library_dir, game_dir]),
        capture_launch_cpu_load_evidence(),
        host_cpu_threads(),
        ActiveLaunchResourceUse {
            session_count: state.sessions().active_session_count().await,
            install_count: state.installs().active_install_count().await,
            memory_allocation_mb: state.sessions().active_memory_allocation_mb().await,
        },
        max_memory_mb,
    );
    let resources_elapsed = resources_started_at.elapsed();
    let failure_memory = state.failure_memory().list();
    let current_at = timestamp_utc();
    let suggested_memory_mb = policy::suggested_max_memory_after_recent_oom(
        max_memory_mb,
        resource_budget.host_total_memory_mb,
        resource_budget.active_memory_allocation_mb,
        &target_version_id,
        is_modded,
    );
    guardian_facts.extend(launch_failure_memory_guardian_facts(
        GuardianLaunchFailureMemoryIntakeRequest {
            entries: &failure_memory,
            instance_id: &instance.id,
            mode: api_guardian_mode(guardian.mode),
            current_at: &current_at,
            current_intent: GuardianLaunchRecoveryCurrentIntent {
                target_version_id: &target_version_id,
                requested_java: &requested_java,
                explicit_jvm_args: &extra_jvm_args,
                requested_preset: &requested_preset,
            },
            current_memory_mb: max_memory_mb,
            suggested_memory_mb,
        },
    ));
    let readiness_started_at = Instant::now();
    let mut readiness = if version_scan_degraded {
        LaunchReadiness {
            launchable: false,
            reasons: vec![LaunchReadinessReason {
                id: LaunchReadinessReasonId::InstalledVersionsDegraded,
                severity: LaunchReadinessSeverity::Blocking,
                message: VERSION_SCAN_DEGRADED_MESSAGE,
            }],
        }
    } else {
        inspect_launch_readiness_structural(
            state.managed_runtime_cache(),
            &LaunchReadinessRequest {
                library_dir: library_dir.to_path_buf(),
                version_id: instance.version_id.clone(),
                requested_java: requested_java.clone(),
                guardian_mode: guardian.mode,
            },
        )
    };
    if integrity_authority_unavailable && readiness.launchable {
        readiness.reasons.push(LaunchReadinessReason {
            id: LaunchReadinessReasonId::IncompleteInstall,
            severity: LaunchReadinessSeverity::Blocking,
            message: "Installation verification is still preparing. Try launching again shortly.",
        });
        readiness.launchable = false;
    }
    let structural_readiness_facts = readiness_guardian_facts(&readiness);
    // Tier 0 facts already reached Guardian directly above. Keep their public
    // readiness projection out of this adapter so size drift is not recast as
    // a legacy checksum fact and the same observation is not admitted twice.
    append_integrity_readiness_reasons(&mut readiness, &integrity_report.facts);
    let readiness_elapsed = readiness_started_at.elapsed();
    guardian_facts.extend(structural_readiness_facts.iter().cloned());
    let guardian_policy_started_at = Instant::now();
    let guardian_outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
        operation_id: None,
        mode: api_guardian_mode(guardian.mode),
        phase: OperationPhase::Validating,
        facts: &guardian_facts,
        readiness: GuardianPreflightReadiness::from_facts(
            readiness.launchable,
            &structural_readiness_facts,
        ),
        resources: preflight_resource_signals(raw_min_memory_mb, max_memory_mb, &resource_budget),
        overrides: preflight_override_signals(&guardian),
        explicit_user_intent: guardian.has_risky_overrides(),
    });
    let preflight_stage_evidence =
        launch_preflight_stage_evidence(&guardian_outcome, &performance_mode);
    apply_guardian_preflight_interventions(
        &guardian_outcome,
        &mut requested_java,
        &mut extra_jvm_args,
        &mut java_probe_receipt,
    );
    let guardian_admission = GuardianLaunchAdmission::preflight(&guardian_outcome);
    let guardian_summary = guardian_summary_from_admission(guardian.mode, &guardian_admission);
    let guardian_policy_elapsed = guardian_policy_started_at.elapsed();

    trace_launch_preflight_facts(LaunchPreflightFactTiming {
        instance_id: &instance.id,
        version_id: &instance.version_id,
        total: started_at.elapsed(),
        senses: LaunchPreflightSenseTimings {
            memory: memory_elapsed,
            installed_versions: installed_versions_elapsed,
            overrides: overrides_elapsed,
            resources: resources_elapsed,
            integrity_tier0: integrity_elapsed,
            readiness: readiness_elapsed,
            guardian_policy: guardian_policy_elapsed,
        },
        version_count: version_records.len(),
        readiness_launchable: readiness.launchable,
        reason_count: readiness.reasons.len(),
        fact_count: guardian_facts.len(),
        guardian_decision: guardian_admission.user_outcome().decision(),
        java_probe_count,
        java_probe_source: java_probe_source.as_str(),
        installed_versions_source: scan_source,
        installed_versions_refresh_count: refresh_count,
        integrity_selected_entry_count: integrity_report.selected_entry_count,
        integrity_skipped_bulk_entry_count: integrity_report.skipped_bulk_entry_count,
        integrity_metadata_lookup_count: integrity_report.metadata_lookup_count,
        integrity_link_lookup_count: integrity_report.link_lookup_count,
        integrity_mtime_observation_count: integrity_report.mtime_observation_count,
        integrity_suppressed_fact_count: integrity_report.suppressed_fact_count,
    });

    LaunchPreflightFacts {
        config: config.clone(),
        max_memory_mb,
        raw_min_memory_mb,
        min_memory_mb,
        requested_java,
        requested_preset,
        extra_jvm_args,
        target_version_id,
        loader,
        is_modded,
        guardian,
        guardian_summary,
        guardian_outcome,
        guardian_admission,
        guardian_facts,
        preflight_stage_evidence,
        readiness,
        resource_budget,
        java_probe_receipt,
    }
}

fn apply_guardian_preflight_interventions(
    outcome: &GuardianPreflightOutcome,
    requested_java: &mut String,
    extra_jvm_args: &mut Vec<String>,
    java_probe_receipt: &mut Option<JavaRuntimeProbeReceipt>,
) {
    for directive in &outcome.directives {
        match directive {
            GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::Preflight,
            } => {
                requested_java.clear();
                *java_probe_receipt = None;
            }
            GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::Preflight,
            } => extra_jvm_args.clear(),
            GuardianDirective::UseManagedJava {
                reason:
                    GuardianManagedJavaReason::PrepareFailure
                    | GuardianManagedJavaReason::StartupRecovery,
            }
            | GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            }
            | GuardianDirective::DowngradeJvmPreset { .. }
            | GuardianDirective::DisableCustomGc => {
                unreachable!("launch preflight emitted a recovery-only directive")
            }
        }
    }
}

impl LaunchPreflightFacts {
    fn into_response(self) -> LaunchPreflightResponse {
        LaunchPreflightResponse {
            status: "ready",
            mode: self.guardian.mode,
            guardian: self.guardian_summary,
            memory: LaunchPreflightMemory {
                max_memory_mb: self.max_memory_mb,
                min_memory_mb: self.min_memory_mb,
                min_clamped: self.raw_min_memory_mb > self.max_memory_mb,
            },
            overrides: LaunchPreflightOverrides {
                java: LaunchPreflightOverride::from_origin(self.guardian.java_override_origin),
                preset: LaunchPreflightOverride::from_origin(self.guardian.preset_override_origin),
                raw_jvm_args: LaunchPreflightOverride::from_origin(
                    self.guardian.raw_jvm_args_origin,
                ),
            },
            readiness: self.readiness,
            guardian_facts: self.guardian_facts,
            resource_budget: LaunchPreflightResourceBudget::from_budget(&self.resource_budget),
        }
    }
}

impl LaunchPreflightOverride {
    fn from_origin(origin: Option<axial_launcher::OverrideOrigin>) -> Self {
        Self {
            present: origin.is_some(),
            origin,
        }
    }
}

#[cfg(test)]
mod tests;
