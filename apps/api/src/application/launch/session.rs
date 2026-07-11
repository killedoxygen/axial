mod auth;
mod overrides;
mod readiness;
mod resources;
mod runtime_repair;

use super::policy;
use super::runner::trace_launch_event;
use crate::application::timing::{
    LaunchPreflightFactTiming, LaunchPreflightResponseTiming, LaunchSessionTiming,
    trace_launch_preflight_facts, trace_launch_preflight_response, trace_launch_session,
};
use crate::application::version::{VERSION_SCAN_DEGRADED_MESSAGE, scan_installed_versions};
use crate::application::{
    LaunchBoundaryStaging, LaunchBoundaryStagingRequest, LaunchInstanceCommand,
    LaunchInstanceStaging, flush_pending_saved_skin_applies_for_launch, stage_launch_boundary,
    stage_launch_instance_command,
};
use crate::guardian::{
    GuardianDecisionKind as ApiGuardianDecisionKind, GuardianFact, GuardianMode as ApiGuardianMode,
    GuardianPreflightDirective, GuardianPreflightOutcome, GuardianPreflightOutcomeRequest,
    GuardianPreflightReadiness, guardian_fact_from_execution, guardian_preflight_outcome,
};
use crate::logging::timestamp_utc;
use crate::state::contracts::OperationPhase;
use crate::state::launch_reports::{LaunchBenchmarkMetadata, LaunchProofResourceBudget};
use crate::state::{AppState, LaunchSessionRecord};
use auth::{LaunchAuthRefreshOptions, resolve_launch_auth_context};
use axial_config::{AppConfig, Instance};
use axial_launcher::{
    GuardianDecision, GuardianMode, GuardianSummary, LaunchGuardianContext, LaunchIntent,
    LaunchReadiness, LaunchReadinessReason, LaunchReadinessReasonId, LaunchReadinessRequest,
    LaunchReadinessSeverity, LaunchState, inspect_launch_readiness, launch_notice,
};
use axum::{Json, http::StatusCode};
use overrides::{
    inspect_explicit_java_override, inspect_explicit_jvm_args, preflight_override_signals,
};
use readiness::readiness_guardian_facts;
#[cfg(test)]
use readiness::readiness_has_managed_runtime_missing;
use resources::{
    ActiveLaunchResourceUse, capture_launch_cpu_load_evidence, capture_launch_disk_evidence,
    capture_launch_memory_evidence, capture_resource_budget_snapshot, host_cpu_threads,
    preflight_resource_signals,
};
#[cfg(test)]
use resources::{LaunchCpuLoadEvidence, LaunchDiskEvidence, LaunchMemoryEvidence, load_to_x100};
use runtime_repair::maybe_repair_managed_runtime_before_launch;
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
    pub application: LaunchInstanceStaging,
    pub boundary: LaunchBoundaryStaging,
    pub instance: Instance,
    pub config: AppConfig,
    pub intent: LaunchIntent,
    pub guardian: GuardianSummary,
    pub launched_at: String,
    pub benchmark: Option<LaunchBenchmarkMetadata>,
    pub resource_budget: Option<LaunchProofResourceBudget>,
}

pub(crate) struct PreparedLaunch {
    pub task: LaunchSessionTask,
}

#[derive(Clone, Debug)]
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
    guardian_facts: Vec<GuardianFact>,
    boundary: LaunchBoundaryStaging,
    readiness: LaunchReadiness,
    resource_budget: LaunchProofResourceBudget,
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

pub(crate) async fn prepare_launch_session(
    state: &AppState,
    payload: LaunchRequest,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    prepare_launch_session_with_auth_refresh(state, payload, None).await
}

async fn prepare_launch_session_with_auth_refresh(
    state: &AppState,
    payload: LaunchRequest,
    auth_refresh: Option<LaunchAuthRefreshOptions>,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    let started_at = Instant::now();
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Axial library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);

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
    let layout_started_at = Instant::now();
    state
        .instances()
        .ensure_instance_layout(&instance.id, Some(&library_dir))
        .map_err(launch_layout_error_response)?;
    let layout_elapsed = layout_started_at.elapsed();
    let game_dir = state.instances().game_dir(&instance.id);

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
        &instance,
        &config,
        &library_dir,
        &game_dir,
        payload.max_memory_mb,
        payload.min_memory_mb,
    )
    .await;
    let preflight_elapsed = preflight_started_at.elapsed();
    let repair_started_at = Instant::now();
    preflight = maybe_repair_managed_runtime_before_launch(
        state,
        preflight,
        &instance,
        &library_dir,
        &game_dir,
        payload.max_memory_mb,
        payload.min_memory_mb,
    )
    .await
    .map_err(launch_journal_error_response)?;
    let repair_elapsed = repair_started_at.elapsed();
    if guardian_preflight_blocks_launch(&preflight.guardian_outcome) {
        trace_launch_session(
            LaunchSessionTiming {
                route: "/api/v1/launch",
                session_id: None,
                instance_id: &instance.id,
                version_id: &instance.version_id,
                total: started_at.elapsed(),
                layout: layout_elapsed,
                auth: auth_elapsed,
                preflight: preflight_elapsed,
                runtime_repair: repair_elapsed,
                insert: None,
                readiness_launchable: preflight.readiness.launchable,
                guardian_decision: preflight.guardian_outcome.user_outcome.decision,
            },
            "launch session blocked by preflight timing",
        );
        return Err(launch_preflight_guardian_error_response(
            preflight.readiness,
            preflight.guardian_summary,
            &preflight.guardian_outcome,
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
            healing: None,
            guardian: serde_json::to_value(&preflight.guardian_summary).ok(),
            outcome: None,
            stages: Vec::new(),
        })
        .await
        .map_err(launch_session_admission_error_response)?;
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
            layout: layout_elapsed,
            auth: auth_elapsed,
            preflight: preflight_elapsed,
            runtime_repair: repair_elapsed,
            insert: Some(insert_elapsed),
            readiness_launchable: preflight.readiness.launchable,
            guardian_decision: preflight.guardian_outcome.user_outcome.decision,
        },
        "launch session preparation timing",
    );

    Ok(PreparedLaunch {
        task: LaunchSessionTask {
            application,
            boundary: preflight.boundary,
            instance: instance.clone(),
            config: preflight.config.clone(),
            intent,
            guardian: preflight.guardian_summary,
            launched_at,
            benchmark: None,
            resource_budget: Some(preflight.resource_budget),
        },
    })
}

fn launch_layout_error_response(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "Could not prepare the instance folder. Check app data permissions and try again."
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

fn launch_preflight_guardian_error_response(
    readiness: LaunchReadiness,
    guardian: GuardianSummary,
    outcome: &GuardianPreflightOutcome,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = if readiness.launchable {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::PRECONDITION_FAILED
    };
    (
        status,
        Json(json!({
            "error": outcome.user_outcome.summary,
            "readiness": readiness,
            "notice": launch_notice(Some(&guardian), None, None, None, None),
            "guardian": guardian,
            "safety": outcome.safety,
        })),
    )
}

pub async fn prepare_launch_preflight(
    state: &AppState,
    instance_id: String,
) -> Result<LaunchPreflightResponse, (StatusCode, Json<serde_json::Value>)> {
    let started_at = Instant::now();
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Axial library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);

    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "instance not found" })),
        )
    })?;
    let game_dir = state.instances().game_dir(&instance.id);
    let config = state.config().current();
    let facts = build_launch_preflight_facts(
        state,
        &instance,
        &config,
        &library_dir,
        &game_dir,
        None,
        None,
    )
    .await;

    trace_launch_preflight_response(LaunchPreflightResponseTiming {
        instance_id: &instance.id,
        version_id: &instance.version_id,
        total: started_at.elapsed(),
        readiness_launchable: facts.readiness.launchable,
        guardian_decision: facts.guardian_outcome.user_outcome.decision,
        reason_count: facts.readiness.reasons.len(),
        fact_count: facts.guardian_facts.len(),
    });

    Ok(facts.into_response())
}

async fn build_launch_preflight_facts(
    state: &AppState,
    instance: &Instance,
    config: &AppConfig,
    library_dir: &Path,
    game_dir: &Path,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
) -> LaunchPreflightFacts {
    let started_at = Instant::now();
    // Preflight is read-only: no session creation, installs, or raw path exposure.
    let memory_started_at = Instant::now();
    let memory_evidence = capture_launch_memory_evidence();
    let memory_elapsed = memory_started_at.elapsed();
    let scan_started_at = Instant::now();
    let installed_versions = scan_installed_versions(library_dir);
    let scan_elapsed = scan_started_at.elapsed();
    let version_scan_degraded = installed_versions.is_degraded();
    let version_records = installed_versions.versions;
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
    let required_java_major = version_record
        .and_then(|version| (version.java_major > 0).then_some(version.java_major as u32));
    let overrides_started_at = Instant::now();
    let mut execution_facts = inspect_explicit_java_override(instance, config, required_java_major)
        .into_iter()
        .flat_map(|inspection| inspection.facts)
        .collect::<Vec<_>>();
    let jvm_args_inspection = inspect_explicit_jvm_args(&instance.extra_jvm_args);
    let mut extra_jvm_args = jvm_args_inspection.args;
    execution_facts.extend(jvm_args_inspection.facts.iter().cloned());
    let overrides_elapsed = overrides_started_at.elapsed();
    let mut guardian_facts = execution_facts
        .iter()
        .map(|fact| guardian_fact_from_execution(fact, OperationPhase::Validating))
        .collect::<Vec<_>>();
    let guardian = LaunchGuardianContext {
        mode: policy::selected_guardian_mode(config),
        java_override_origin: policy::java_override_origin(instance, config),
        preset_override_origin: policy::preset_override_origin(instance, config),
        raw_jvm_args_origin: policy::raw_jvm_args_origin(instance),
    };
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
    let readiness_started_at = Instant::now();
    let readiness = if version_scan_degraded {
        LaunchReadiness {
            launchable: false,
            reasons: vec![LaunchReadinessReason {
                id: LaunchReadinessReasonId::InstalledVersionsDegraded,
                severity: LaunchReadinessSeverity::Blocking,
                message: VERSION_SCAN_DEGRADED_MESSAGE,
            }],
        }
    } else {
        inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.to_path_buf(),
            version_id: instance.version_id.clone(),
            requested_java: requested_java.clone(),
            guardian_mode: guardian.mode,
        })
    };
    let readiness_elapsed = readiness_started_at.elapsed();
    let readiness_facts = readiness_guardian_facts(&readiness);
    guardian_facts.extend(readiness_facts.iter().cloned());
    let guardian_started_at = Instant::now();
    let boundary = stage_launch_boundary(LaunchBoundaryStagingRequest::new(
        application_guardian_mode(guardian.mode),
        OperationPhase::Validating,
        &guardian_facts,
        &performance_mode,
    ));
    let guardian_outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
        operation_id: None,
        mode: application_guardian_mode(guardian.mode),
        phase: OperationPhase::Validating,
        facts: &guardian_facts,
        readiness: GuardianPreflightReadiness::from_facts(readiness.launchable, &readiness_facts),
        resources: preflight_resource_signals(raw_min_memory_mb, max_memory_mb, &resource_budget),
        overrides: preflight_override_signals(&guardian),
        explicit_user_intent: guardian.has_risky_overrides(),
    });
    apply_guardian_preflight_interventions(
        &guardian_outcome,
        &mut requested_java,
        &mut extra_jvm_args,
    );
    let guardian_summary =
        guardian_summary_from_preflight_outcome(guardian.mode, &guardian_outcome);
    let guardian_elapsed = guardian_started_at.elapsed();

    trace_launch_preflight_facts(LaunchPreflightFactTiming {
        instance_id: &instance.id,
        version_id: &instance.version_id,
        total: started_at.elapsed(),
        memory: memory_elapsed,
        scan: scan_elapsed,
        overrides: overrides_elapsed,
        resources: resources_elapsed,
        readiness: readiness_elapsed,
        guardian: guardian_elapsed,
        version_count: version_records.len(),
        readiness_launchable: readiness.launchable,
        reason_count: readiness.reasons.len(),
        fact_count: guardian_facts.len(),
        guardian_decision: guardian_outcome.user_outcome.decision,
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
        guardian_facts,
        boundary,
        readiness,
        resource_budget,
    }
}

fn apply_guardian_preflight_interventions(
    outcome: &GuardianPreflightOutcome,
    requested_java: &mut String,
    extra_jvm_args: &mut Vec<String>,
) {
    for directive in &outcome.directives {
        match directive {
            GuardianPreflightDirective::UseManagedJavaForAttempt => requested_java.clear(),
            GuardianPreflightDirective::StripExplicitJvmArgsForAttempt => extra_jvm_args.clear(),
        }
    }
}

fn application_guardian_mode(mode: GuardianMode) -> ApiGuardianMode {
    match mode {
        GuardianMode::Managed => ApiGuardianMode::Managed,
        GuardianMode::Custom => ApiGuardianMode::Custom,
    }
}

fn guardian_preflight_blocks_launch(outcome: &GuardianPreflightOutcome) -> bool {
    matches!(
        outcome.user_outcome.decision,
        ApiGuardianDecisionKind::Block | ApiGuardianDecisionKind::AskUser
    )
}

fn guardian_summary_from_preflight_outcome(
    mode: GuardianMode,
    outcome: &GuardianPreflightOutcome,
) -> GuardianSummary {
    let public_details = legacy_preflight_public_lines(outcome);
    GuardianSummary {
        mode,
        decision: legacy_preflight_decision(outcome.user_outcome.decision),
        message: Some(outcome.user_outcome.summary.clone()),
        details: public_details.clone(),
        guidance: public_details,
        interventions: Vec::new(),
    }
}

fn legacy_preflight_public_lines(outcome: &GuardianPreflightOutcome) -> Vec<String> {
    let mut lines = Vec::new();
    for value in outcome
        .user_outcome
        .details
        .iter()
        .chain(outcome.user_outcome.guidance.iter())
    {
        if !value.trim().is_empty() && !lines.iter().any(|existing| existing == value) {
            lines.push(value.clone());
        }
    }
    lines
}

fn legacy_preflight_decision(decision: ApiGuardianDecisionKind) -> GuardianDecision {
    match decision {
        ApiGuardianDecisionKind::Allow | ApiGuardianDecisionKind::RecordOnly => {
            GuardianDecision::Allowed
        }
        ApiGuardianDecisionKind::Warn => GuardianDecision::Warned,
        ApiGuardianDecisionKind::Block | ApiGuardianDecisionKind::AskUser => {
            GuardianDecision::Blocked
        }
        _ => GuardianDecision::Intervened,
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
