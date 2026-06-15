use super::policy;
use super::runner::trace_launch_event;
use crate::application::{
    LaunchBoundaryStaging, LaunchBoundaryStagingRequest, LaunchInstanceCommand,
    LaunchInstanceStaging, stage_launch_boundary, stage_launch_instance_command,
};
use crate::execution::jvm::{JvmArgsInspection, JvmArgsInspectionRequest, inspect_jvm_args};
use crate::execution::runtime::{
    JavaOverrideInspection, ManagedRuntimeRoot, ManagedRuntimeVerificationRequest,
    inspect_java_override_value, verify_managed_runtime,
};
use crate::guardian::{
    FactReliability, GuardianConfidence, GuardianDomain, GuardianFact, GuardianFactId,
    GuardianManagedRuntimeRepairRequest, GuardianRepairOutcome, GuardianRepairStatus,
    GuardianSeverity, diagnose_facts, execute_managed_runtime_ready_marker_repair,
    guardian_fact_from_execution,
};
use crate::logging::timestamp_utc;
use crate::routes::{
    accounts,
    auth::{AuthRefreshFailure, refresh_active_auth},
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::launch_reports::{LaunchBenchmarkMetadata, LaunchProofResourceBudget};
use crate::state::{
    ActiveMinecraftAccountState, AppState, LaunchSessionRecord, LauncherAccountKind,
    LauncherAccountRecord,
};
use axum::{Json, http::StatusCode};
use croopor_config::{AppConfig, AppPaths, Instance, LAUNCH_AUTH_MODE_ONLINE, validate_username};
use croopor_launcher::{
    GuardianDecision, GuardianMode, GuardianSummary, LAUNCH_DISK_HEADROOM_MB,
    LAUNCH_MEMORY_HEADROOM_MB, LaunchAuthContext, LaunchCpuLoadWarningFacts, LaunchFailureClass,
    LaunchGuardianContext, LaunchIntent, LaunchNotice, LaunchNoticeTone, LaunchReadiness,
    LaunchReadinessReasonId, LaunchReadinessRequest, LaunchReadinessSeverity,
    LaunchResourceWarningFacts, LaunchState, LaunchWarningFacts, failure_class_name,
    inspect_launch_readiness, launch_notice, summarize_launch_warnings,
};
use croopor_minecraft::{preferred_runtime_component, resolve_version, scan_versions};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

#[derive(Clone, Debug, Deserialize)]
pub(super) struct LaunchRequest {
    pub instance_id: String,
    pub username: Option<String>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub client_started_at_ms: Option<i64>,
}

pub(super) struct LaunchSessionTask {
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

pub(super) struct PreparedLaunch {
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
    guardian_facts: Vec<GuardianFact>,
    boundary: LaunchBoundaryStaging,
    readiness: LaunchReadiness,
    resource_budget: LaunchProofResourceBudget,
}

#[derive(Clone, Copy)]
struct LaunchAuthRefreshOptions;

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightResponse {
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
pub(super) struct LaunchPreflightMemory {
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub min_clamped: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightOverrides {
    pub java: LaunchPreflightOverride,
    pub preset: LaunchPreflightOverride,
    pub raw_jvm_args: LaunchPreflightOverride,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightOverride {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<croopor_launcher::OverrideOrigin>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightResourceBudget {
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

pub(super) async fn prepare_launch_session(
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
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Croopor library is not configured" })),
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
    state
        .instances()
        .ensure_instance_layout(&instance.id, Some(&library_dir))
        .map_err(launch_layout_error_response)?;
    let game_dir = state.instances().game_dir(&instance.id);

    let config = state.config().current();
    let requested_offline_username = payload
        .username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(config.username.as_str());
    let requested_offline_username = validate_username(requested_offline_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let active_account =
        accounts::sync_active_offline_account_from_username(state, &requested_offline_username)
            .map_err(launch_account_store_error_response)?;
    let requested_username = active_account
        .as_ref()
        .filter(|account| account.kind == LauncherAccountKind::Offline)
        .map(|account| account.display_name.as_str())
        .unwrap_or(requested_offline_username.as_str())
        .to_string();
    let offline_username = validate_username(&requested_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let auth = if let Some(auth_refresh) = auth_refresh {
        launch_auth_context_for_config_with_refresh(
            state,
            &config,
            active_account.as_ref(),
            &offline_username,
            auth_refresh,
        )
        .await?
    } else {
        launch_auth_context_for_config(state, &config, active_account.as_ref(), &offline_username)
            .await?
    };
    let online_launch = active_account
        .as_ref()
        .is_some_and(|account| account.kind == LauncherAccountKind::Microsoft)
        || (active_account.is_none() && config.launch_auth_mode == LAUNCH_AUTH_MODE_ONLINE);
    if online_launch {
        super::super::skin::flush_pending_saved_skin_applies_for_launch(state).await?;
    }
    let username = offline_username.clone();
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
    preflight = maybe_repair_managed_runtime_before_launch(
        state,
        preflight,
        &instance,
        &library_dir,
        &game_dir,
        payload.max_memory_mb,
        payload.min_memory_mb,
    )
    .await;
    if !preflight.readiness.launchable {
        return Err(launch_readiness_error_response(
            preflight.readiness,
            Some(preflight.guardian_summary),
        ));
    }
    if preflight.guardian_summary.decision == GuardianDecision::Blocked {
        return Err(launch_guardian_block_error_response(
            preflight.readiness,
            preflight.guardian_summary,
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
        auth,
        requested_java: preflight.requested_java.clone(),
        requested_preset: preflight.requested_preset.clone(),
        extra_jvm_args: preflight.extra_jvm_args.clone(),
        max_memory_mb: preflight.max_memory_mb,
        min_memory_mb: preflight.min_memory_mb,
        resolution: policy::selected_resolution(&instance, &config),
        launcher_name: "croopor".to_string(),
        launcher_version: state.version().to_string(),
        game_dir: Some(game_dir),
        guardian: preflight.guardian.clone(),
        performance_mode: policy::selected_performance_mode(&instance, &config),
    };

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
        .await;
    trace_launch_event(
        &session_id.0,
        &format!(
            "launch requested for instance {} version {} client_started_at_ms={:?}",
            instance.id, instance.version_id, payload.client_started_at_ms
        ),
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

fn launch_readiness_error_response(
    readiness: LaunchReadiness,
    guardian: Option<GuardianSummary>,
) -> (StatusCode, Json<serde_json::Value>) {
    let notice = guardian
        .as_ref()
        .and_then(|guardian| launch_notice(Some(guardian), None, None, None, None));
    (
        StatusCode::PRECONDITION_FAILED,
        Json(json!({
            "error": "Installed version is not ready to launch",
            "readiness": readiness,
            "guardian": guardian,
            "notice": notice,
        })),
    )
}

fn launch_guardian_block_error_response(
    readiness: LaunchReadiness,
    guardian: GuardianSummary,
) -> (StatusCode, Json<serde_json::Value>) {
    let error = guardian
        .message
        .clone()
        .unwrap_or_else(|| "Guardian blocked launch preparation".to_string());
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({
            "error": error,
            "readiness": readiness,
            "notice": launch_notice(Some(&guardian), None, None, None, None),
            "guardian": guardian,
        })),
    )
}

pub(super) async fn prepare_launch_preflight(
    state: &AppState,
    instance_id: String,
) -> Result<LaunchPreflightResponse, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Croopor library is not configured" })),
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
    // Preflight is read-only: no session creation, installs, Java probes, or raw path exposure.
    let memory_evidence = capture_launch_memory_evidence();
    let version_records = scan_versions(library_dir).unwrap_or_default();
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
    let requested_java = policy::selected_java_override(instance, config);
    let requested_preset = policy::selected_jvm_preset(instance, config);
    let mut execution_facts = inspect_explicit_java_override(instance, config)
        .into_iter()
        .flat_map(|inspection| inspection.facts)
        .collect::<Vec<_>>();
    let jvm_args_inspection = inspect_explicit_jvm_args(&instance.extra_jvm_args);
    execution_facts.extend(jvm_args_inspection.facts.iter().cloned());
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
    let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
        library_dir: library_dir.to_path_buf(),
        version_id: instance.version_id.clone(),
        requested_java: requested_java.clone(),
        guardian_mode: guardian.mode,
    });
    guardian_facts.extend(readiness_guardian_facts(&readiness));
    let boundary = stage_launch_boundary(LaunchBoundaryStagingRequest::new(
        application_guardian_mode(guardian.mode),
        OperationPhase::Validating,
        &guardian_facts,
        &performance_mode,
    ));
    let mut guardian_summary = guardian_summary_for_preflight(
        raw_min_memory_mb,
        max_memory_mb,
        &resource_budget,
        &guardian,
    );
    append_launch_guardian_guidance(&mut guardian_summary, &guardian_facts);
    block_guardian_for_launch_readiness(&mut guardian_summary, &readiness);

    LaunchPreflightFacts {
        config: config.clone(),
        max_memory_mb,
        raw_min_memory_mb,
        min_memory_mb,
        requested_java,
        requested_preset,
        extra_jvm_args: jvm_args_inspection.args,
        target_version_id,
        loader,
        is_modded,
        guardian,
        guardian_summary,
        guardian_facts,
        boundary,
        readiness,
        resource_budget,
    }
}

async fn maybe_repair_managed_runtime_before_launch(
    state: &AppState,
    mut preflight: LaunchPreflightFacts,
    instance: &Instance,
    library_dir: &Path,
    game_dir: &Path,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
) -> LaunchPreflightFacts {
    if preflight.guardian.mode != GuardianMode::Managed
        || !readiness_has_managed_runtime_missing(&preflight.readiness)
    {
        return preflight;
    }

    let Some(candidate) = managed_runtime_ready_marker_repair_candidate(
        state.config().paths(),
        library_dir,
        instance,
    ) else {
        return preflight;
    };
    let Ok(runtime_root) = ManagedRuntimeRoot::from_app_paths(
        state.config().paths(),
        &candidate.runtime_root,
        &candidate.java_executable,
    ) else {
        return preflight;
    };

    let verification = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(
        runtime_root.target().clone(),
        &candidate.runtime_root,
        &candidate.java_executable,
    ));
    let Err(verification_error) = verification else {
        return preflight;
    };
    let guardian_facts = verification_error
        .facts
        .iter()
        .map(|fact| guardian_fact_from_execution(fact, OperationPhase::Validating))
        .collect::<Vec<_>>();
    let performance_mode = policy::selected_performance_mode(instance, &preflight.config);
    let repair_boundary = stage_launch_boundary(LaunchBoundaryStagingRequest::new(
        application_guardian_mode(preflight.guardian.mode),
        OperationPhase::Validating,
        &guardian_facts,
        &performance_mode,
    ));

    let outcome =
        execute_managed_runtime_ready_marker_repair(GuardianManagedRuntimeRepairRequest {
            decision: &repair_boundary.guardian_decision,
            runtime_root,
            journals: state.journals().as_ref(),
            failure_memory: state.failure_memory().as_ref(),
            observed_at: timestamp_utc().as_str(),
            suppression_until_on_failure: None,
        });

    match outcome.status {
        GuardianRepairStatus::Repaired => {
            let mut repaired = build_launch_preflight_facts(
                state,
                instance,
                &preflight.config,
                library_dir,
                game_dir,
                requested_max_memory_mb,
                requested_min_memory_mb,
            )
            .await;
            mark_guardian_runtime_repair_success(&mut repaired.guardian_summary, &outcome);
            repaired
        }
        GuardianRepairStatus::Blocked
        | GuardianRepairStatus::Failed
        | GuardianRepairStatus::Suppressed => {
            block_guardian_for_runtime_repair_outcome(&mut preflight.guardian_summary, &outcome);
            preflight
        }
        GuardianRepairStatus::NotNeeded => preflight,
    }
}

struct ManagedRuntimeRepairCandidate {
    runtime_root: PathBuf,
    java_executable: PathBuf,
}

fn managed_runtime_ready_marker_repair_candidate(
    paths: &AppPaths,
    library_dir: &Path,
    instance: &Instance,
) -> Option<ManagedRuntimeRepairCandidate> {
    let version = resolve_version(library_dir, &instance.version_id).ok()?;
    let component = preferred_runtime_component(&version.java_version);
    let runtime_root = paths.config_dir.join("runtimes").join(component);
    if !runtime_root.exists() || runtime_root.join(".croopor-ready").is_file() {
        return None;
    }
    let java_executable = managed_runtime_java_executable(&runtime_root);
    if !java_executable.is_file() {
        return None;
    }
    Some(ManagedRuntimeRepairCandidate {
        runtime_root,
        java_executable,
    })
}

fn managed_runtime_java_executable(runtime_root: &Path) -> PathBuf {
    runtime_root
        .join("bin")
        .join(if cfg!(target_os = "windows") {
            "javaw.exe"
        } else {
            "java"
        })
}

fn readiness_has_managed_runtime_missing(readiness: &LaunchReadiness) -> bool {
    readiness
        .reasons
        .iter()
        .any(|reason| reason.id == LaunchReadinessReasonId::ManagedRuntimeMissing)
}

fn readiness_guardian_facts(readiness: &LaunchReadiness) -> Vec<GuardianFact> {
    readiness
        .reasons
        .iter()
        .filter_map(|reason| {
            let id = readiness_guardian_fact_id(reason.id)?;
            Some(GuardianFact {
                operation_id: None,
                id: GuardianFactId::new(id),
                domain: readiness_guardian_domain(reason.id),
                phase: OperationPhase::Validating,
                reliability: readiness_guardian_fact_reliability(reason.id),
                severity: Some(readiness_guardian_severity(reason.severity)),
                confidence: Some(GuardianConfidence::Confirmed),
                ownership: readiness_guardian_ownership(reason.id),
                target: Some(TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    readiness_guardian_target_kind(reason.id),
                    readiness_guardian_target_id(reason.id),
                    readiness_guardian_ownership(reason.id),
                )),
                fields: Vec::new(),
            })
        })
        .collect()
}

fn readiness_guardian_fact_id(reason: LaunchReadinessReasonId) -> Option<&'static str> {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing => Some("version_json_missing"),
        LaunchReadinessReasonId::ParentVersionMissing => Some("parent_version_missing"),
        LaunchReadinessReasonId::IncompleteInstall => Some("incomplete_install"),
        LaunchReadinessReasonId::ClientJarMissing => Some("client_jar_missing"),
        LaunchReadinessReasonId::LibrariesMissing => Some("libraries_missing"),
        LaunchReadinessReasonId::AssetIndexMissing => Some("asset_index_missing"),
        LaunchReadinessReasonId::ManagedRuntimeMissing => Some("managed_runtime_missing"),
        LaunchReadinessReasonId::JavaOverrideMissing => Some("java_override_missing"),
    }
}

fn readiness_guardian_domain(reason: LaunchReadinessReasonId) -> GuardianDomain {
    match reason {
        LaunchReadinessReasonId::ManagedRuntimeMissing
        | LaunchReadinessReasonId::JavaOverrideMissing => GuardianDomain::Runtime,
        _ => GuardianDomain::Install,
    }
}

fn readiness_guardian_severity(severity: LaunchReadinessSeverity) -> GuardianSeverity {
    match severity {
        LaunchReadinessSeverity::Blocking => GuardianSeverity::Blocking,
        LaunchReadinessSeverity::Recoverable => GuardianSeverity::Recoverable,
    }
}

fn readiness_guardian_ownership(reason: LaunchReadinessReasonId) -> OwnershipClass {
    match reason {
        LaunchReadinessReasonId::JavaOverrideMissing => OwnershipClass::UserOwned,
        _ => OwnershipClass::LauncherManaged,
    }
}

fn readiness_guardian_target_kind(reason: LaunchReadinessReasonId) -> TargetKind {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing
        | LaunchReadinessReasonId::ParentVersionMissing
        | LaunchReadinessReasonId::IncompleteInstall => TargetKind::Version,
        LaunchReadinessReasonId::ClientJarMissing
        | LaunchReadinessReasonId::LibrariesMissing
        | LaunchReadinessReasonId::AssetIndexMissing => TargetKind::Artifact,
        LaunchReadinessReasonId::ManagedRuntimeMissing => TargetKind::Runtime,
        LaunchReadinessReasonId::JavaOverrideMissing => TargetKind::Config,
    }
}

fn readiness_guardian_target_id(reason: LaunchReadinessReasonId) -> &'static str {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing => "version_json_missing",
        LaunchReadinessReasonId::ParentVersionMissing => "parent_version_missing",
        LaunchReadinessReasonId::IncompleteInstall => "incomplete_install",
        LaunchReadinessReasonId::ClientJarMissing => "client_jar",
        LaunchReadinessReasonId::LibrariesMissing => "libraries",
        LaunchReadinessReasonId::AssetIndexMissing => "asset_index",
        LaunchReadinessReasonId::ManagedRuntimeMissing => "managed_runtime",
        LaunchReadinessReasonId::JavaOverrideMissing => "explicit_java_override",
    }
}

fn readiness_guardian_fact_reliability(reason: LaunchReadinessReasonId) -> FactReliability {
    match reason {
        LaunchReadinessReasonId::IncompleteInstall => FactReliability::DirectStructured,
        _ => FactReliability::ExpectedMarkerAbsence,
    }
}

fn mark_guardian_runtime_repair_success(
    summary: &mut GuardianSummary,
    _outcome: &GuardianRepairOutcome,
) {
    let previous_details = summary.details.clone();
    let previous_guidance = summary.guidance.clone();
    summary.decision = GuardianDecision::Intervened;
    summary.message = Some("Guardian repaired launch state before launch.".to_string());
    summary.details.clear();
    push_unique_summary_detail(
        &mut summary.details,
        "Guardian repaired the managed Java runtime before launch.",
    );
    for detail in previous_details {
        push_unique_summary_detail(&mut summary.details, &detail);
    }
    for detail in &previous_guidance {
        push_unique_summary_detail(&mut summary.details, detail);
    }
    summary.guidance = previous_guidance;
}

fn block_guardian_for_runtime_repair_outcome(
    summary: &mut GuardianSummary,
    outcome: &GuardianRepairOutcome,
) {
    let reason = match outcome.status {
        GuardianRepairStatus::Suppressed => {
            "Guardian suppressed managed Java runtime repair because the same repair failed recently."
        }
        GuardianRepairStatus::Failed => {
            "Guardian could not repair the managed Java runtime automatically."
        }
        GuardianRepairStatus::Blocked => {
            "Guardian blocked managed Java runtime repair because it was not safe to apply."
        }
        GuardianRepairStatus::NotNeeded | GuardianRepairStatus::Repaired => {
            "Guardian did not need managed Java runtime repair."
        }
    };
    let mut guidance = summary.guidance.clone();
    push_unique_guidance(
        &mut guidance,
        "Reinstall or repair the affected version/runtime before launching again.",
    );
    summary.block_with_reason_and_guidance(reason, guidance);
}

fn block_guardian_for_launch_readiness(summary: &mut GuardianSummary, readiness: &LaunchReadiness) {
    let Some(reason) = readiness
        .reasons
        .iter()
        .find(|reason| {
            reason.severity == LaunchReadinessSeverity::Blocking
                && readiness_guardian_fact_id(reason.id).is_some()
        })
        .map(|reason| reason.id)
    else {
        return;
    };

    let mut guidance = summary.guidance.clone();
    push_unique_guidance(&mut guidance, readiness_guardian_guidance(reason));
    summary.block_with_reason_and_guidance(readiness_guardian_block_reason(reason), guidance);
}

fn readiness_guardian_block_reason(reason: LaunchReadinessReasonId) -> &'static str {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing => {
            "Guardian blocked launch because installed version metadata is missing."
        }
        LaunchReadinessReasonId::ParentVersionMissing => {
            "Guardian blocked launch because parent version metadata is missing."
        }
        LaunchReadinessReasonId::IncompleteInstall => {
            "Guardian blocked launch because the install is incomplete."
        }
        LaunchReadinessReasonId::ClientJarMissing => {
            "Guardian blocked launch because client game files are missing."
        }
        LaunchReadinessReasonId::LibrariesMissing => {
            "Guardian blocked launch because required libraries are missing."
        }
        LaunchReadinessReasonId::AssetIndexMissing => {
            "Guardian blocked launch because the asset index is missing."
        }
        LaunchReadinessReasonId::JavaOverrideMissing => {
            "Guardian blocked launch because the selected Java override is unavailable."
        }
        _ => "Guardian blocked launch because readiness failed.",
    }
}

fn readiness_guardian_guidance(reason: LaunchReadinessReasonId) -> &'static str {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing
        | LaunchReadinessReasonId::ParentVersionMissing
        | LaunchReadinessReasonId::IncompleteInstall
        | LaunchReadinessReasonId::ClientJarMissing
        | LaunchReadinessReasonId::LibrariesMissing
        | LaunchReadinessReasonId::AssetIndexMissing => {
            "Install or repair the affected version before launching again."
        }
        LaunchReadinessReasonId::JavaOverrideMissing => {
            "Choose a valid Java runtime or switch back to Managed Java before launching again."
        }
        _ => "Repair the affected launch state before launching again.",
    }
}

fn push_unique_summary_detail(details: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() && !details.iter().any(|detail| detail == value) {
        details.push(value.to_string());
    }
}

fn inspect_explicit_java_override(
    instance: &Instance,
    config: &AppConfig,
) -> Option<JavaOverrideInspection> {
    if !instance.java_path.is_empty() {
        return Some(inspect_java_override_value(
            None,
            java_override_target("instance_java_override"),
            &instance.java_path,
        ));
    }
    if !config.java_path_override.is_empty() {
        return Some(inspect_java_override_value(
            None,
            java_override_target("global_java_override"),
            &config.java_path_override,
        ));
    }
    None
}

fn java_override_target(id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Config,
        id,
        OwnershipClass::UserOwned,
    )
}

fn application_guardian_mode(mode: GuardianMode) -> crate::guardian::GuardianMode {
    match mode {
        GuardianMode::Managed => crate::guardian::GuardianMode::Managed,
        GuardianMode::Custom => crate::guardian::GuardianMode::Custom,
    }
}

fn inspect_explicit_jvm_args(raw_args: &str) -> JvmArgsInspection {
    if raw_args.trim().is_empty() {
        return JvmArgsInspection {
            args: Vec::new(),
            facts: Vec::new(),
        };
    }
    inspect_jvm_args(JvmArgsInspectionRequest::new(
        explicit_jvm_args_target(),
        raw_args,
    ))
}

fn explicit_jvm_args_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Config,
        "explicit_jvm_args",
        OwnershipClass::UserOwned,
    )
}

fn append_launch_guardian_guidance(summary: &mut GuardianSummary, facts: &[GuardianFact]) {
    if facts.is_empty() {
        return;
    }

    let diagnoses = diagnose_facts(facts, OperationPhase::Validating);
    let mut guidance = Vec::new();
    for diagnosis in diagnoses {
        match diagnosis.id.as_str() {
            "java_override_unavailable" => push_unique_guidance(
                &mut guidance,
                "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
            "jvm_args_malformed" => push_unique_guidance(
                &mut guidance,
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ),
            "jvm_arg_unsupported" => push_unique_guidance(
                &mut guidance,
                "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
            ),
            "jvm_arg_unsafe_override" => push_unique_guidance(
                &mut guidance,
                "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
            ),
            _ => {}
        }
    }
    if !guidance.is_empty() {
        summary.warn_with_guidance(guidance);
    }
}

fn push_unique_guidance(guidance: &mut Vec<String>, value: &str) {
    if !guidance.iter().any(|existing| existing == value) {
        guidance.push(value.to_string());
    }
}

async fn launch_auth_context_for_config(
    state: &AppState,
    config: &AppConfig,
    active_account: Option<&LauncherAccountRecord>,
    offline_username: &str,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    launch_auth_context_for_config_with_refresh(
        state,
        config,
        active_account,
        offline_username,
        LaunchAuthRefreshOptions,
    )
    .await
}

async fn launch_auth_context_for_config_with_refresh(
    state: &AppState,
    config: &AppConfig,
    active_account: Option<&LauncherAccountRecord>,
    offline_username: &str,
    auth_refresh: LaunchAuthRefreshOptions,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    // Online launches try one active-account refresh before returning a user-facing auth block.
    if let Some(account) = active_account {
        return match account.kind {
            LauncherAccountKind::Offline => Ok(LaunchAuthContext::offline(&account.display_name)),
            LauncherAccountKind::Microsoft => {
                let Some(login_id) = account.login_id.as_deref() else {
                    return Err(online_auth_refresh_unavailable_response(
                        "refresh_failed",
                        "account_login_missing",
                    ));
                };
                match state.auth_logins().switch_active_account(login_id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(online_auth_refresh_unavailable_response(
                            "refresh_failed",
                            "account_auth_missing",
                        ));
                    }
                    Err(_) => {
                        return Err(online_auth_refresh_unavailable_response(
                            "refresh_failed",
                            "account_selection_failed",
                        ));
                    }
                }
                online_launch_auth_context_with_refresh(state, auth_refresh).await
            }
        };
    }

    if config.launch_auth_mode != LAUNCH_AUTH_MODE_ONLINE {
        return Ok(LaunchAuthContext::offline(offline_username));
    }

    online_launch_auth_context_with_refresh(state, auth_refresh).await
}

async fn online_launch_auth_context_with_refresh(
    state: &AppState,
    _auth_refresh: LaunchAuthRefreshOptions,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    if let Some(auth) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .and_then(online_launch_auth_context)
    {
        return Ok(auth);
    }

    refresh_active_auth(state.auth_logins())
        .await
        .map_err(online_auth_refresh_failure_response)?;

    state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .and_then(online_launch_auth_context)
        .ok_or_else(|| {
            online_auth_refresh_unavailable_response("refresh_failed", "refreshed_account_unusable")
        })
}

fn launch_account_store_error_response(
    error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": error.to_string(),
            "failure_class": failure_class_name(LaunchFailureClass::AuthModeIncompatible),
            "status": "account_persistence_failed",
        })),
    )
}

fn online_launch_auth_context(
    account_state: ActiveMinecraftAccountState,
) -> Option<LaunchAuthContext> {
    let account = account_state.account;
    if !account.owns_minecraft_java
        || account.access_token.trim().is_empty()
        || account.profile.name.trim().is_empty()
        || account.profile.id.trim().is_empty()
    {
        return None;
    }

    Some(LaunchAuthContext {
        player_name: account.profile.name,
        uuid: account.profile.id,
        access_token: account.access_token,
        client_id: String::new(),
        xuid: String::new(),
        user_type: "msa".to_string(),
    })
}

fn online_auth_refresh_failure_response(
    error: AuthRefreshFailure,
) -> (StatusCode, Json<serde_json::Value>) {
    online_auth_unavailable_response_with_refresh(Some((
        error.launch_status_id(),
        error.launch_reason_id(),
    )))
}

fn online_auth_refresh_unavailable_response(
    refresh_status: &'static str,
    refresh_reason: &'static str,
) -> (StatusCode, Json<serde_json::Value>) {
    online_auth_unavailable_response_with_refresh(Some((refresh_status, refresh_reason)))
}

fn online_auth_unavailable_response_with_refresh(
    refresh: Option<(&'static str, &'static str)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut response = json!({
        "error": "Online launch requires an active verified Minecraft Java account",
        "failure_class": failure_class_name(LaunchFailureClass::AuthModeIncompatible),
        "launch_auth_mode": LAUNCH_AUTH_MODE_ONLINE,
        "online_mode_ready": false,
    });
    if let Some((refresh_status, refresh_reason)) = refresh {
        response["auth_refresh_status"] = json!(refresh_status);
        response["auth_refresh_reason"] = json!(refresh_reason);
    }
    response["notice"] = json!(online_auth_launch_notice(refresh));

    (StatusCode::PRECONDITION_FAILED, Json(response))
}

fn online_auth_launch_notice(refresh: Option<(&'static str, &'static str)>) -> LaunchNotice {
    let reason = refresh.map(|(_, reason)| reason).unwrap_or_default();
    let sign_in_required = matches!(
        reason,
        "refresh_token_missing" | "refresh_token_rejected" | "refresh_state_unavailable"
    ) || refresh
        .map(|(status, _)| status == "sign_in_required")
        .unwrap_or(false);
    let message = if sign_in_required {
        "Online launch needs you to sign in again."
    } else {
        "Online launch could not verify your Minecraft account."
    };
    let first_detail = if sign_in_required {
        match reason {
            "refresh_token_missing" => {
                "Croopor could not refresh the Microsoft session because the saved sign-in is missing or expired."
            }
            "refresh_token_rejected" => "Microsoft rejected the saved sign-in session.",
            "refresh_state_unavailable" => "Croopor could not read the saved sign-in session.",
            _ => "Croopor could not use the saved Microsoft session for Online launch.",
        }
    } else {
        match reason {
            "auth_chain_failed" => {
                "Croopor refreshed Microsoft sign-in, but Minecraft account verification did not complete."
            }
            "client_id_missing" => "Microsoft sign-in is not configured for this build.",
            "client_build" | "token_client_unavailable" => {
                "Croopor could not start Microsoft sign-in refresh."
            }
            "oauth_refresh_failed"
            | "token_endpoint_unreachable"
            | "token_endpoint_rejected"
            | "token_endpoint_unavailable"
            | "token_endpoint_parse_failed" => {
                "Microsoft sign-in refresh is unavailable or did not complete."
            }
            "refreshed_account_unusable" => {
                "The refreshed account could not be used for a verified Minecraft Java launch."
            }
            _ => "Croopor could not verify the Microsoft account for Online launch.",
        }
    };
    let second_detail = if sign_in_required {
        "Sign in again from Accounts, then retry Online launch."
    } else {
        "Refresh or re-verify the account from Accounts, then retry Online launch."
    };
    let details = vec![
        first_detail.to_string(),
        second_detail.to_string(),
        "Offline launch remains available for singleplayer and offline-mode servers.".to_string(),
    ];
    LaunchNotice {
        message: message.to_string(),
        detail: details.first().cloned(),
        details,
        tone: LaunchNoticeTone::Error,
    }
}

fn guardian_summary_for_preflight(
    raw_min_memory_mb: i32,
    max_memory_mb: i32,
    resource_budget: &LaunchProofResourceBudget,
    guardian: &LaunchGuardianContext,
) -> GuardianSummary {
    summarize_launch_warnings(
        guardian,
        &LaunchWarningFacts {
            raw_min_memory_mb,
            max_memory_mb,
            resource: launch_resource_warning_facts(resource_budget),
        },
    )
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
    fn from_origin(origin: Option<croopor_launcher::OverrideOrigin>) -> Self {
        Self {
            present: origin.is_some(),
            origin,
        }
    }
}

impl LaunchPreflightResourceBudget {
    fn from_budget(resource_budget: &LaunchProofResourceBudget) -> Self {
        Self {
            active_session_count: resource_budget.active_session_count,
            active_install_count: resource_budget.active_install_count,
            active_memory_allocation_mb: resource_budget.active_memory_allocation_mb,
            requested_memory_mb: resource_budget.requested_memory_mb,
            estimated_remaining_memory_mb: resource_budget.estimated_remaining_memory_mb,
            memory_pressure: resource_budget.memory_pressure,
            cpu_pressure: resource_budget.cpu_pressure,
            install_pressure: resource_budget.install_pressure,
            disk_pressure: resource_budget.disk_pressure,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchMemoryEvidence {
    host_total_memory_mb: Option<u64>,
    host_available_memory_mb: Option<u64>,
    host_used_memory_mb: Option<u64>,
    launcher_process_memory_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchDiskEvidence {
    launch_disk_available_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchCpuLoadEvidence {
    host_cpu_load_1m_x100: Option<u64>,
    host_cpu_load_5m_x100: Option<u64>,
    host_cpu_load_15m_x100: Option<u64>,
}

fn capture_launch_memory_evidence() -> LaunchMemoryEvidence {
    let mut system = System::new();
    system.refresh_memory();
    let launcher_process_memory_mb = current_process_memory_mb(&mut system);

    LaunchMemoryEvidence {
        host_total_memory_mb: bytes_to_positive_mb(system.total_memory()),
        host_available_memory_mb: bytes_to_positive_mb(system.available_memory()),
        host_used_memory_mb: bytes_to_positive_mb(system.used_memory()),
        launcher_process_memory_mb,
    }
}

fn current_process_memory_mb(system: &mut System) -> Option<u64> {
    let pid = get_current_pid().ok()?;
    let process_refresh = ProcessRefreshKind::nothing().with_memory().without_tasks();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, process_refresh);
    system
        .process(pid)
        .and_then(|process| bytes_to_positive_mb(process.memory()))
}

fn bytes_to_positive_mb(value: u64) -> Option<u64> {
    let value = value / (1024 * 1024);
    (value > 0).then_some(value)
}

fn capture_launch_disk_evidence<'a>(
    candidate_paths: impl IntoIterator<Item = &'a Path>,
) -> LaunchDiskEvidence {
    let disks = Disks::new_with_refreshed_list();
    let launch_disk_available_mb = candidate_paths
        .into_iter()
        .filter_map(|path| disk_available_mb_for_path(&disks, path))
        .min();

    LaunchDiskEvidence {
        launch_disk_available_mb,
    }
}

fn capture_launch_cpu_load_evidence() -> LaunchCpuLoadEvidence {
    #[cfg(unix)]
    {
        let load = System::load_average();
        LaunchCpuLoadEvidence {
            host_cpu_load_1m_x100: load_to_x100(load.one),
            host_cpu_load_5m_x100: load_to_x100(load.five),
            host_cpu_load_15m_x100: load_to_x100(load.fifteen),
        }
    }

    #[cfg(not(unix))]
    {
        LaunchCpuLoadEvidence::default()
    }
}

#[cfg(unix)]
fn load_to_x100(value: f64) -> Option<u64> {
    if value.is_finite() && value >= 0.0 {
        Some((value * 100.0).round().clamp(0.0, u64::MAX as f64) as u64)
    } else {
        None
    }
}

fn disk_available_mb_for_path(disks: &Disks, path: &Path) -> Option<u64> {
    let path = path.canonicalize().ok()?;
    disks
        .list()
        .iter()
        .filter_map(|disk| {
            let mount_point = disk.mount_point().canonicalize().ok()?;
            path.starts_with(&mount_point).then(|| {
                (
                    mount_point.components().count(),
                    disk.available_space() / (1024 * 1024),
                )
            })
        })
        .max_by_key(|(mount_depth, _)| *mount_depth)
        .map(|(_, available_mb)| available_mb)
}

fn host_cpu_threads() -> Option<usize> {
    std::thread::available_parallelism().ok().map(usize::from)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ActiveLaunchResourceUse {
    session_count: usize,
    install_count: usize,
    memory_allocation_mb: u64,
}

fn capture_resource_budget_snapshot(
    memory_evidence: LaunchMemoryEvidence,
    disk_evidence: LaunchDiskEvidence,
    cpu_load_evidence: LaunchCpuLoadEvidence,
    host_cpu_threads: Option<usize>,
    active: ActiveLaunchResourceUse,
    requested_allocation_mb: i32,
) -> LaunchProofResourceBudget {
    // Captured before launch work starts so Guardian and proof records use the same pressure view.
    let requested_memory_mb = positive_i32(requested_allocation_mb);
    let warning_facts = LaunchResourceWarningFacts {
        host_total_memory_mb: memory_evidence.host_total_memory_mb,
        host_cpu_threads,
        cpu_load: LaunchCpuLoadWarningFacts {
            host_cpu_load_1m_x100: cpu_load_evidence.host_cpu_load_1m_x100,
            host_cpu_load_5m_x100: cpu_load_evidence.host_cpu_load_5m_x100,
            host_cpu_load_15m_x100: cpu_load_evidence.host_cpu_load_15m_x100,
        },
        active_session_count: active.session_count,
        active_install_count: active.install_count,
        active_memory_allocation_mb: active.memory_allocation_mb,
        requested_memory_mb,
        launch_disk_available_mb: disk_evidence.launch_disk_available_mb,
        memory_headroom_mb: LAUNCH_MEMORY_HEADROOM_MB,
        launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
    };
    LaunchProofResourceBudget {
        host_total_memory_mb: memory_evidence.host_total_memory_mb,
        host_available_memory_mb: memory_evidence.host_available_memory_mb,
        host_used_memory_mb: memory_evidence.host_used_memory_mb,
        host_cpu_threads,
        host_cpu_load_1m_x100: cpu_load_evidence.host_cpu_load_1m_x100,
        host_cpu_load_5m_x100: cpu_load_evidence.host_cpu_load_5m_x100,
        host_cpu_load_15m_x100: cpu_load_evidence.host_cpu_load_15m_x100,
        launcher_process_memory_mb: memory_evidence.launcher_process_memory_mb,
        active_session_count: active.session_count,
        active_install_count: active.install_count,
        active_memory_allocation_mb: active.memory_allocation_mb,
        requested_memory_mb,
        estimated_remaining_memory_mb: estimated_remaining_memory_mb(
            memory_evidence.host_total_memory_mb,
            active.memory_allocation_mb,
            requested_memory_mb,
        ),
        memory_headroom_mb: LAUNCH_MEMORY_HEADROOM_MB,
        memory_pressure: warning_facts.memory_pressure(),
        cpu_pressure: warning_facts.cpu_pressure(),
        install_pressure: warning_facts.install_pressure(),
        launch_disk_available_mb: disk_evidence.launch_disk_available_mb,
        launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
        disk_pressure: warning_facts.disk_pressure(),
    }
}

fn launch_resource_warning_facts(
    resource_budget: &LaunchProofResourceBudget,
) -> LaunchResourceWarningFacts {
    LaunchResourceWarningFacts {
        host_total_memory_mb: resource_budget.host_total_memory_mb,
        host_cpu_threads: resource_budget.host_cpu_threads,
        cpu_load: LaunchCpuLoadWarningFacts {
            host_cpu_load_1m_x100: resource_budget.host_cpu_load_1m_x100,
            host_cpu_load_5m_x100: resource_budget.host_cpu_load_5m_x100,
            host_cpu_load_15m_x100: resource_budget.host_cpu_load_15m_x100,
        },
        active_session_count: resource_budget.active_session_count,
        active_install_count: resource_budget.active_install_count,
        active_memory_allocation_mb: resource_budget.active_memory_allocation_mb,
        requested_memory_mb: resource_budget.requested_memory_mb,
        launch_disk_available_mb: resource_budget.launch_disk_available_mb,
        memory_headroom_mb: resource_budget.memory_headroom_mb,
        launch_disk_headroom_mb: resource_budget.launch_disk_headroom_mb,
    }
}

fn estimated_remaining_memory_mb(
    total_memory_mb: Option<u64>,
    active_allocation_mb: u64,
    requested_allocation_mb: Option<i32>,
) -> Option<i64> {
    let requested_allocation_mb = u64::try_from(requested_allocation_mb?).ok()?;
    // Signed estimate preserves overcommit amount instead of saturating negative headroom to zero.
    let remaining = i128::from(total_memory_mb?)
        - i128::from(active_allocation_mb)
        - i128::from(requested_allocation_mb);
    Some(remaining.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

fn positive_i32(value: i32) -> Option<i32> {
    (value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::failure_memory::FailureMemoryActionOutcome;
    use crate::state::{
        AppStateInit, AuthLoginMinecraftProfile, InstallStore, NewAuthLoginMinecraftAccount,
        NewAuthLoginMsaToken, SessionStore,
    };
    use axum::Json;
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{
        GuardianDecision, LaunchReadinessReason, LaunchReadinessReasonId, LaunchReadinessSeverity,
        OverrideOrigin, SessionId,
    };
    use croopor_performance::PerformanceManager;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    #[test]
    fn launch_layout_error_response_keeps_500_and_hides_unix_path_material() {
        let (status, Json(body)) = launch_layout_error_response(
            "/Users/alice/Library/Application Support/Croopor/instances/survival: Permission denied (os error 13)",
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            serde_json::json!({
                "error": "Could not prepare the instance folder. Check app data permissions and try again."
            })
        );
        let body = body.to_string();
        for fragment in [
            "/Users/alice",
            "Library/Application Support",
            "instances/survival",
            "Permission denied",
            "os error 13",
        ] {
            assert!(
                !body.contains(fragment),
                "launch layout error exposed raw fragment {fragment}"
            );
        }
    }

    #[test]
    fn launch_layout_error_response_hides_windows_path_material_and_raw_io_text() {
        let (status, Json(body)) = launch_layout_error_response(
            r"C:\Users\Alice\AppData\Roaming\Croopor\instances\creative: Access is denied. Read-only file system (os error 5)",
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["error"],
            "Could not prepare the instance folder. Check app data permissions and try again."
        );
        let body = body.to_string();
        for fragment in [
            r"C:\Users\Alice",
            "AppData",
            r"instances\creative",
            "Access is denied",
            "Read-only file system",
            "os error 5",
        ] {
            assert!(
                !body.contains(fragment),
                "launch layout error exposed raw fragment {fragment}"
            );
        }
    }

    #[tokio::test]
    async fn prepare_launch_session_ensures_instance_layout_before_building_intent() {
        let fixture = TestFixture::new("prepare-ensures-layout");
        fixture.write_ready_install("1.21.1");
        fs::write(
            fixture.paths.library_dir.join("options.txt"),
            "shared options",
        )
        .expect("write options");
        fs::write(
            fixture.paths.library_dir.join("servers.dat"),
            "shared servers",
        )
        .expect("write servers");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let game_dir = fixture.state.instances().game_dir(&instance_id);
        let _ = fs::remove_dir_all(game_dir.join("screenshots"));
        let _ = fs::remove_dir_all(game_dir.join("logs"));

        let prepared = prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        .expect("prepare launch session");

        assert_eq!(
            prepared.task.application.command.kind,
            crate::state::contracts::CommandKind::LaunchInstance
        );
        assert_eq!(
            prepared
                .task
                .application
                .result
                .payload
                .session_id
                .as_deref(),
            Some(prepared.task.intent.session_id.as_str())
        );
        assert_eq!(
            prepared
                .task
                .application
                .result
                .carriers
                .session
                .as_ref()
                .and_then(|session| session.state.as_deref()),
            Some("queued")
        );
        assert_eq!(
            prepared.task.boundary.guardian_decision.mode,
            crate::guardian::GuardianMode::Managed
        );
        assert_eq!(prepared.task.boundary.performance_mode, "managed");
        assert_eq!(prepared.task.intent.game_dir, Some(game_dir.clone()));
        assert_eq!(prepared.task.intent.auth.player_name, "Player");
        assert_eq!(
            prepared.task.intent.auth.uuid,
            croopor_minecraft::offline_uuid("Player")
        );
        assert_eq!(prepared.task.intent.auth.access_token, "null");
        assert_eq!(prepared.task.intent.auth.user_type, "legacy");
        assert!(game_dir.join("screenshots").is_dir());
        assert!(game_dir.join("logs").is_dir());
        assert_eq!(
            fs::read_to_string(game_dir.join("options.txt")).expect("read options"),
            "shared options"
        );
        assert_eq!(
            fs::read_to_string(game_dir.join("servers.dat")).expect("read servers"),
            "shared servers"
        );
    }

    #[tokio::test]
    async fn prepare_launch_session_syncs_active_offline_account_from_config_username() {
        let fixture = TestFixture::new("prepare-syncs-offline-account-name");
        fixture.write_ready_install("1.21.1");
        fixture
            .state
            .accounts()
            .create_offline_account("OldName")
            .expect("create offline account");
        let mut config = fixture.state.config().current();
        config.username = "NewName".to_string();
        fixture
            .state
            .config()
            .replace_in_memory(config)
            .expect("set config username");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id,
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        .expect("prepare launch session");

        assert_eq!(prepared.task.intent.auth.player_name, "NewName");
        assert_eq!(prepared.task.intent.username, "NewName");
        let active = fixture
            .state
            .accounts()
            .active_account()
            .expect("active account")
            .expect("active account");
        assert_eq!(active.display_name, "NewName");
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_invalid_offline_request_username_as_bad_request() {
        let fixture = TestFixture::new("prepare-invalid-offline-name");
        fixture
            .state
            .accounts()
            .create_offline_account("LocalUser")
            .expect("create offline account");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id,
                username: Some("Bad Name!".to_string()),
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("invalid username should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0["error"],
            "Letters, numbers, and underscores only."
        );
    }

    #[tokio::test]
    async fn prepare_launch_session_uses_online_auth_context_from_active_minecraft_account() {
        let fixture = TestFixture::new("prepare-online-auth");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.add_active_minecraft_account(true).await;
        let prepared = fixture
            .prepare(instance_id, None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.config.username, "Player");
        assert_eq!(prepared.task.intent.username, "Player");
        assert_eq!(prepared.task.intent.auth.player_name, "ProfileName");
        assert_eq!(
            prepared.task.intent.auth.uuid,
            "4f9c7f7d0b1245d9a5c2f03a8c120001"
        );
        assert_eq!(
            prepared.task.intent.auth.access_token,
            "minecraft-access-token"
        );
        assert_eq!(prepared.task.intent.auth.user_type, "msa");
        assert_eq!(prepared.task.intent.auth.client_id, "");
        assert_eq!(prepared.task.intent.auth.xuid, "");
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_online_auth_missing_refresh_token_boundedly() {
        let fixture = TestFixture::new("prepare-online-auth-no-refresh");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let error = match fixture.prepare(instance_id, None).await {
            Ok(_) => panic!("online auth without refresh token should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
        assert_eq!(error.1.0["launch_auth_mode"], "online");
        assert_eq!(error.1.0["online_mode_ready"], false);
        assert_eq!(error.1.0["auth_refresh_status"], "sign_in_required");
        assert_eq!(error.1.0["auth_refresh_reason"], "refresh_token_missing");
        assert_launch_error_is_token_safe(&error.1.0);
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_online_auth_without_verified_account_boundedly() {
        let fixture = TestFixture::new("prepare-online-auth-missing");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match fixture.prepare(instance_id.clone(), None).await {
            Ok(_) => panic!("online auth without account should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
        assert_eq!(error.1.0["launch_auth_mode"], "online");
        assert_eq!(error.1.0["online_mode_ready"], false);
        let text = error.1.0.to_string();
        for material in [
            "minecraft-access-token",
            "msa-access-token",
            "provider-secret-payload",
        ] {
            assert!(
                !text.contains(material),
                "public launch error exposed sensitive material {material}"
            );
        }
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_online_auth_without_java_ownership() {
        let fixture = TestFixture::new("prepare-online-auth-unowned");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.add_active_minecraft_account(false).await;

        let error = match fixture.prepare(instance_id, None).await {
            Ok(_) => panic!("online auth without ownership should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
        assert_eq!(error.1.0["online_mode_ready"], false);
        let text = error.1.0.to_string();
        assert!(!text.contains("minecraft-access-token"));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_same_instance_active_launch() {
        let fixture = TestFixture::new("prepare-active-conflict");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture
            .state
            .sessions()
            .insert(LaunchSessionRecord {
                session_id: SessionId("active-session".to_string()),
                instance_id: instance_id.clone(),
                version_id: "1.21.1".to_string(),
                launched_at: Some(timestamp_utc()),
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
                guardian: None,
                outcome: None,
                stages: Vec::new(),
            })
            .await;

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id,
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("active instance should conflict"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "instance already has an active session" })
        );
    }

    #[tokio::test]
    async fn omitted_request_memory_uses_backend_derived_defaults_for_fresh_builtin_global() {
        let fixture = TestFixture::new("prepare-derived-memory-defaults");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let config = fixture.state.config().current();
        let expected_defaults = policy::derived_launch_memory_defaults(
            &fixture
                .state
                .instances()
                .get(&instance_id)
                .expect("instance"),
            &config,
            None,
            None,
            None,
            capture_launch_memory_evidence().host_total_memory_mb,
        );

        let prepared = fixture
            .prepare_with_memory(instance_id, None, None)
            .await
            .expect("prepare launch session");

        if let Some(defaults) = expected_defaults {
            assert_eq!(prepared.task.intent.max_memory_mb, defaults.max_memory_mb);
            assert_eq!(prepared.task.intent.min_memory_mb, defaults.min_memory_mb);
            assert_ne!(
                prepared.task.intent.min_memory_mb,
                AppConfig::default().min_memory_mb
            );
        } else {
            assert_eq!(prepared.task.intent.max_memory_mb, config.max_memory_mb);
            assert_eq!(prepared.task.intent.min_memory_mb, config.min_memory_mb);
        }
    }

    #[tokio::test]
    async fn launch_preflight_ready_payload_for_managed_instance_does_not_create_session() {
        let fixture = TestFixture::new("preflight-managed-ready");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.status, "ready");
        assert_eq!(preflight.mode, GuardianMode::Managed);
        assert_eq!(preflight.guardian.mode, GuardianMode::Managed);
        assert!(!preflight.overrides.java.present);
        assert_eq!(preflight.overrides.java.origin, None);
        assert!(!preflight.overrides.preset.present);
        assert_eq!(preflight.overrides.preset.origin, None);
        assert!(!preflight.overrides.raw_jvm_args.present);
        assert_eq!(preflight.overrides.raw_jvm_args.origin, None);
        assert!(preflight.memory.max_memory_mb > 0);
        assert!(preflight.memory.min_memory_mb >= 0);
        assert!(!preflight.memory.min_clamped);
        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_version_json() {
        let fixture = TestFixture::new("preflight-readiness-missing-version-json");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_eq!(preflight.readiness.reasons.len(), 1);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
        assert_guardian_fact(&preflight, "version_json_missing");
        assert!(preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because installed version metadata is missing."
        }));
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_client_jar() {
        let fixture = TestFixture::new("preflight-readiness-missing-client-jar");
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }),
        );
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::ClientJarMissing);
        let runtime_reason =
            readiness_reason(&preflight, LaunchReadinessReasonId::ManagedRuntimeMissing);
        assert_eq!(
            runtime_reason.severity,
            LaunchReadinessSeverity::Recoverable
        );
        assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
        assert_guardian_fact(&preflight, "client_jar_missing");
        assert_guardian_fact(&preflight, "managed_runtime_missing");
        assert!(preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because client game files are missing."
        }));
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_libraries_as_guardian_fact() {
        let fixture = TestFixture::new("preflight-readiness-missing-libraries");
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": [{
                    "name": "com.example:demo:1.0.0",
                    "downloads": {
                        "artifact": {
                            "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                            "url": "https://example.invalid/demo-1.0.0.jar"
                        }
                    }
                }]
            }),
        );
        let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("write client jar");
        fixture.write_ready_runtime("java-runtime-delta");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::LibrariesMissing);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
        assert_guardian_fact(&preflight, "libraries_missing");
        assert!(preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because required libraries are missing."
        }));
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_asset_index_as_guardian_fact() {
        let fixture = TestFixture::new("preflight-readiness-missing-asset-index");
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": { "id": "test-assets" },
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }),
        );
        let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("write client jar");
        fixture.write_ready_runtime("java-runtime-delta");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::AssetIndexMissing);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
        assert_guardian_fact(&preflight, "asset_index_missing");
        assert!(preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because the asset index is missing."
        }));
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_managed_runtime_as_recoverable_fact() {
        let fixture = TestFixture::new("preflight-readiness-missing-managed-runtime");
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "croopor-test-runtime-missing", "majorVersion": 21 },
                "libraries": []
            }),
        );
        let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("write client jar");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(preflight.readiness.launchable);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Allowed);
        assert_eq!(
            readiness_reason(&preflight, LaunchReadinessReasonId::ManagedRuntimeMissing).severity,
            LaunchReadinessSeverity::Recoverable
        );
        let fact = guardian_fact(&preflight, "managed_runtime_missing");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
        assert_eq!(fact.severity, Some(GuardianSeverity::Recoverable));
    }

    #[tokio::test]
    async fn launch_preparation_repairs_managed_runtime_ready_marker_before_blocking_readiness() {
        let fixture = TestFixture::new("prepare-repairs-runtime-ready-marker");
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }),
        );
        let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
        let runtime_root = fixture.write_global_runtime_without_ready_marker("java-runtime-delta");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let instance = fixture
            .state
            .instances()
            .get(&instance_id)
            .expect("instance");
        let config = fixture.state.config().current();
        let game_dir = fixture.state.instances().game_dir(&instance.id);

        let preflight = build_launch_preflight_facts(
            &fixture.state,
            &instance,
            &config,
            &fixture.paths.library_dir,
            &game_dir,
            None,
            None,
        )
        .await;
        assert!(
            readiness_has_managed_runtime_missing(&preflight.readiness),
            "missing managed runtime readiness reason: {:?}",
            preflight.readiness.reasons
        );

        let repaired = maybe_repair_managed_runtime_before_launch(
            &fixture.state,
            preflight,
            &instance,
            &fixture.paths.library_dir,
            &game_dir,
            None,
            None,
        )
        .await;

        assert!(runtime_root.join(".croopor-ready").is_file());
        assert_eq!(
            repaired.guardian_summary.decision,
            GuardianDecision::Intervened
        );
        assert!(repaired.guardian_summary.details.iter().any(|detail| {
            detail == "Guardian repaired the managed Java runtime before launch."
        }));
        let memory = fixture.state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
    }

    #[tokio::test]
    async fn launch_preparation_blocks_when_managed_runtime_repair_is_suppressed() {
        let fixture = TestFixture::new("prepare-blocks-suppressed-runtime-repair");
        let component = "croopor-test-runtime-suppressed";
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": component, "majorVersion": 21 },
                "libraries": []
            }),
        );
        let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
        let runtime_root = fixture.write_global_runtime_without_ready_marker(component);
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let instance = fixture
            .state
            .instances()
            .get(&instance_id)
            .expect("instance");
        let config = fixture.state.config().current();
        let game_dir = fixture.state.instances().game_dir(&instance.id);
        let preflight = build_launch_preflight_facts(
            &fixture.state,
            &instance,
            &config,
            &fixture.paths.library_dir,
            &game_dir,
            None,
            None,
        )
        .await;

        let repaired = maybe_repair_managed_runtime_before_launch(
            &fixture.state,
            preflight,
            &instance,
            &fixture.paths.library_dir,
            &game_dir,
            None,
            None,
        )
        .await;
        assert_eq!(
            repaired.guardian_summary.decision,
            GuardianDecision::Intervened
        );
        fs::remove_file(runtime_root.join(".croopor-ready")).expect("remove ready marker");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("suppressed repair should block launch preparation"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.1.0["guardian"]["decision"], "blocked");
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["severity"],
            "recoverable"
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
        let memory = fixture.state.failure_memory().list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_incomplete_install_marker() {
        let fixture = TestFixture::new("preflight-readiness-incomplete-install");
        fixture.write_ready_install("1.21.1");
        fs::write(
            fixture
                .paths
                .library_dir
                .join("versions")
                .join("1.21.1")
                .join(".incomplete"),
            b"installing",
        )
        .expect("incomplete marker");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_eq!(preflight.readiness.reasons.len(), 1);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::IncompleteInstall);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
        assert_guardian_fact(&preflight, "incomplete_install");
        assert!(preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because the install is incomplete."
        }));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_missing_version_json_with_guardian_block() {
        let fixture = TestFixture::new("prepare-rejects-missing-version-json");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("missing version metadata should not queue"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            error.1.0["error"],
            "Installed version is not ready to launch"
        );
        assert_eq!(error.1.0["readiness"]["launchable"], false);
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["id"],
            "version_json_missing"
        );
        assert_eq!(error.1.0["guardian"]["decision"], "blocked");
        assert!(
            error.1.0["guardian"]["details"]
                .as_array()
                .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                    == Some(
                        "Guardian blocked launch because installed version metadata is missing."
                    )))
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
        let payload = error.1.0.to_string();
        assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_incomplete_install_without_session() {
        let fixture = TestFixture::new("prepare-rejects-incomplete-install");
        fixture.write_ready_install("1.21.1");
        fs::write(
            fixture
                .paths
                .library_dir
                .join("versions")
                .join("1.21.1")
                .join(".incomplete"),
            b"installing",
        )
        .expect("incomplete marker");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("incomplete install should not queue"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            error.1.0["error"],
            "Installed version is not ready to launch"
        );
        assert_eq!(error.1.0["readiness"]["launchable"], false);
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["id"],
            "incomplete_install"
        );
        assert_eq!(error.1.0["guardian"]["decision"], "blocked");
        assert!(
            error.1.0["guardian"]["details"]
                .as_array()
                .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                    == Some("Guardian blocked launch because the install is incomplete.")))
        );
        assert_eq!(
            error.1.0["notice"]["message"],
            "Guardian blocked an unsafe launch setup."
        );
        assert!(
            error.1.0["notice"]["details"]
                .as_array()
                .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                    == Some("Guardian blocked launch because the install is incomplete.")))
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
        let payload = error.1.0.to_string();
        assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
        assert!(!payload.contains(".incomplete"));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_incomplete_parent_without_session() {
        let fixture = TestFixture::new("prepare-rejects-incomplete-parent");
        fixture.write_ready_install("1.21.1");
        fixture.write_child_version("fabric-loader-0.16.10-1.21.1", "1.21.1");
        fs::write(
            fixture
                .paths
                .library_dir
                .join("versions")
                .join("1.21.1")
                .join(".incomplete"),
            b"installing",
        )
        .expect("incomplete marker");
        let instance_id = fixture.add_instance("Modded", "fabric-loader-0.16.10-1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("incomplete parent install should not queue"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["id"],
            "incomplete_install"
        );
        assert_eq!(error.1.0["guardian"]["decision"], "blocked");
        assert!(
            error.1.0["guardian"]["details"]
                .as_array()
                .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                    == Some("Guardian blocked launch because the install is incomplete.")))
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }

    #[tokio::test]
    async fn launch_preflight_custom_override_warns_with_bounded_override_payload() {
        let fixture = TestFixture::new("preflight-custom-bounded");
        fixture.set_guardian_mode("custom");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path.clone();
            instance.extra_jvm_args = "-Dtoken=secret-token -XX:+UseZGC".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.status, "ready");
        assert_eq!(preflight.mode, GuardianMode::Custom);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert_eq!(
            preflight.overrides.java.origin,
            Some(OverrideOrigin::Instance)
        );
        assert_eq!(
            preflight.overrides.raw_jvm_args.origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(preflight.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(preflight.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable."));

        let payload = serde_json::to_string(&preflight).expect("serialize preflight");
        assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
        assert!(!payload.contains("-Dtoken"));
        assert!(!payload.contains("secret-token"));
        assert!(!payload.contains("requested_java"));
        assert!(!payload.contains("requested_preset"));
        assert!(!payload.contains("java_path"));
        assert!(!payload.contains("command"));
        assert!(!payload.contains("username"));
        for reason in &preflight.readiness.reasons {
            assert!(
                !reason
                    .message
                    .contains(&fixture.root.to_string_lossy().to_string())
            );
            assert!(!reason.message.contains("secret-token"));
        }
    }

    #[tokio::test]
    async fn launch_preflight_bad_custom_java_override_blocks_with_guardian_fact() {
        let fixture = TestFixture::new("preflight-bad-custom-java-block");
        fixture.set_guardian_mode("custom");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/Users/SecretUser/.jdks/manual/bin/java".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::JavaOverrideMissing);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
        let fact = guardian_fact(&preflight, "java_override_missing");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
        assert_eq!(fact.ownership, OwnershipClass::UserOwned);
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("explicit_java_override")
        );
        assert!(preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because the selected Java override is unavailable."
        }));

        let payload = serde_json::to_string(&preflight).expect("serialize preflight");
        assert!(!payload.contains("/Users/SecretUser"));
        assert!(!payload.contains("manual/bin/java"));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_bad_custom_java_override_with_guardian_block() {
        let fixture = TestFixture::new("prepare-bad-custom-java-block");
        fixture.set_guardian_mode("custom");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/Users/SecretUser/.jdks/manual/bin/java".to_string();
        });

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("bad custom Java override should not queue"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["readiness"]["launchable"], false);
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["id"],
            "java_override_missing"
        );
        assert_eq!(error.1.0["guardian"]["decision"], "blocked");
        assert!(
            error.1.0["guardian"]["details"]
                .as_array()
                .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                    == Some(
                        "Guardian blocked launch because the selected Java override is unavailable."
                    )))
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
        let payload = error.1.0.to_string();
        assert!(!payload.contains("/Users/SecretUser"));
        assert!(!payload.contains("manual/bin/java"));
    }

    #[tokio::test]
    async fn launch_preflight_malformed_jvm_args_exposes_redacted_guardian_fact() {
        let fixture = TestFixture::new("preflight-malformed-jvm-fact");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.extra_jvm_args =
                r#"-Xmx2G "unterminated C:\Users\Alice\.jdks\java.exe"#.to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert!(preflight.guardian.guidance.iter().any(|detail| {
            detail == "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch."
        }));
        let fact = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "jvm_args_parse_failed")
            .expect("jvm parse fact");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("explicit_jvm_args")
        );
        let payload = serde_json::to_string(&preflight).expect("serialize preflight");
        let lower = payload.to_ascii_lowercase();
        assert!(!lower.contains("alice"));
        assert!(!lower.contains(".jdks"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("unterminated"));
    }

    #[tokio::test]
    async fn launch_preflight_unsupported_jvm_gc_flags_exposes_guardian_fact() {
        let fixture = TestFixture::new("preflight-unsupported-jvm-fact");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.extra_jvm_args = "-XX:+UseZGC -Dtoken=secret-token".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert!(preflight.guardian.guidance.iter().any(|detail| {
            detail == "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails."
        }));
        let fact = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "jvm_arg_unsupported_gc")
            .expect("unsupported jvm fact");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("explicit_jvm_args")
        );

        let payload = serde_json::to_string(&preflight).expect("serialize preflight");
        let lower = payload.to_ascii_lowercase();
        assert!(!lower.contains("-xx:+usezgc"));
        assert!(!lower.contains("-dtoken"));
        assert!(!lower.contains("secret-token"));
    }

    #[tokio::test]
    async fn launch_preflight_undefined_java_override_exposes_guardian_fact() {
        let fixture = TestFixture::new("preflight-undefined-java-fact");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "undefined".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert_eq!(
            preflight.overrides.java.origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(preflight.guardian.guidance.iter().any(|detail| {
            detail == "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch."
        }));
        let fact = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "java_override_undefined_sentinel")
            .expect("java sentinel fact");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("instance_java_override")
        );
        assert!(
            fact.fields
                .iter()
                .any(|field| { field.key == "sentinel" && field.value == "undefined" })
        );
    }

    #[tokio::test]
    async fn launch_preflight_null_java_override_exposes_guardian_fact() {
        let fixture = TestFixture::new("preflight-null-java-fact");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "null".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        let fact = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "java_override_undefined_sentinel")
            .expect("java sentinel fact");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
        assert!(
            fact.fields
                .iter()
                .any(|field| { field.key == "sentinel" && field.value == "null" })
        );
    }

    #[tokio::test]
    async fn launch_preflight_blank_explicit_java_override_exposes_guardian_fact() {
        let fixture = TestFixture::new("preflight-empty-java-fact");
        fixture.set_global_java_override("/opt/java/bin/java");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "   ".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert_eq!(
            preflight.overrides.java.origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(preflight.guardian_facts.iter().any(|fact| {
            fact.id.as_str() == "java_override_empty"
                && fact
                    .target
                    .as_ref()
                    .is_some_and(|target| target.id == "instance_java_override")
        }));
    }

    #[tokio::test]
    async fn launch_preflight_memory_clamp_warning_is_reflected() {
        let fixture = TestFixture::new("preflight-memory-clamp");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.max_memory_mb = 1024;
            instance.min_memory_mb = 2048;
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.memory.max_memory_mb, 1024);
        assert_eq!(preflight.memory.min_memory_mb, 1024);
        assert!(preflight.memory.min_clamped);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&preflight.guardian);
    }

    #[tokio::test]
    async fn launch_preflight_resource_warning_path_is_reflected() {
        let fixture = TestFixture::new("preflight-resource-warning");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(
            preflight.resource_budget.active_session_count,
            fixture.state.sessions().active_session_count().await
        );
        assert_eq!(
            preflight.resource_budget.active_install_count,
            fixture.state.installs().active_install_count().await
        );
        assert_eq!(
            preflight.resource_budget.requested_memory_mb,
            Some(preflight.memory.max_memory_mb)
        );
    }

    #[tokio::test]
    async fn custom_mode_with_java_override_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-java-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path.clone();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_java, java_path);
        assert_eq!(
            prepared.task.intent.guardian.java_override_origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Switch Guardian back to Managed if you want Croopor to adjust unsafe choices."));
    }

    #[tokio::test]
    async fn custom_mode_with_raw_jvm_args_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-jvm-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.extra_jvm_args = "-XX:+UseZGC -Ddemo=true".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(
            prepared.task.intent.extra_jvm_args,
            vec!["-XX:+UseZGC", "-Ddemo=true"]
        );
        assert_eq!(
            prepared.task.intent.guardian.raw_jvm_args_origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail
                    == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.")
        );
    }

    #[tokio::test]
    async fn malformed_raw_jvm_args_warn_before_queue_without_changing_fallback_args() {
        let fixture = TestFixture::new("prepare-malformed-jvm-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.extra_jvm_args = r#"-Xmx2G "unterminated"#.to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail
                    == "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.")
        );
        assert_eq!(
            prepared.task.intent.extra_jvm_args,
            vec!["-Xmx2G", "\"unterminated"]
        );
        let guardian = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
        assert!(!guardian.to_ascii_lowercase().contains("-xmx"));
        assert!(!guardian.contains("unterminated"));
    }

    #[tokio::test]
    async fn custom_mode_with_instance_jvm_preset_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-preset-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.jvm_preset = "graalvm".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_preset, "graalvm");
        assert_eq!(
            prepared.task.intent.guardian.preset_override_origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected JVM preset for this launch."));
        assert!(prepared.task.guardian.details.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected JVM preset for this launch."));
    }

    #[tokio::test]
    async fn custom_mode_with_global_jvm_preset_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-global-preset-warning");
        fixture.set_guardian_mode("custom");
        fixture.set_global_jvm_preset("performance");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_preset, "performance");
        assert_eq!(
            prepared.task.intent.guardian.preset_override_origin,
            Some(OverrideOrigin::Global)
        );
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected JVM preset for this launch."));
    }

    #[tokio::test]
    async fn managed_mode_with_manual_overrides_skips_custom_warning_at_queue_time() {
        let fixture = TestFixture::new("prepare-managed-overrides-no-custom-warning");
        fixture.set_guardian_mode("managed");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path.clone();
            instance.jvm_preset = "graalvm".to_string();
            instance.extra_jvm_args = "-XX:+UseZGC".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert!(
            !prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail.starts_with("Guardian Custom mode will keep"))
        );
        assert!(
            !prepared
                .task
                .guardian
                .details
                .iter()
                .any(|detail| detail.starts_with("Guardian Custom mode will keep"))
        );
        assert_eq!(prepared.task.intent.requested_java, java_path);
        assert_eq!(prepared.task.intent.requested_preset, "graalvm");
        assert_eq!(prepared.task.intent.extra_jvm_args, vec!["-XX:+UseZGC"]);
    }

    #[tokio::test]
    async fn instance_min_above_max_warns_and_clamps_intent_min_to_max() {
        let fixture = TestFixture::new("prepare-instance-memory-clamp-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.max_memory_mb = 1024;
            instance.min_memory_mb = 2048;
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn request_min_above_request_max_warns_for_api_callers() {
        let fixture = TestFixture::new("prepare-request-memory-clamp-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(2048))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn normal_min_at_or_below_max_does_not_add_clamp_warning() {
        let fixture = TestFixture::new("prepare-no-memory-clamp-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(4096), Some(1024))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 4096);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_no_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn low_max_memory_warns_without_changing_intent_memory_values() {
        let fixture = TestFixture::new("prepare-low-max-memory-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(512))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 512);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_low_memory_allocation_warning(&prepared.task.guardian, 1024);
        assert_no_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn memory_clamp_warning_merges_with_custom_override_warning() {
        let fixture = TestFixture::new("prepare-memory-clamp-custom-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(2048))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&prepared.task.guardian);
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(prepared.task.guardian.details.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
    }

    #[tokio::test]
    async fn low_memory_warning_merges_with_custom_override_warning() {
        let fixture = TestFixture::new("prepare-low-memory-custom-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(512))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 512);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_low_memory_allocation_warning(&prepared.task.guardian, 1024);
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(prepared.task.guardian.details.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
    }

    #[tokio::test]
    async fn memory_warning_and_custom_override_warning_merge_before_queue() {
        let fixture = TestFixture::new("prepare-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });

        let prepared = fixture
            .prepare(instance_id.clone(), Some(i32::MAX))
            .await
            .expect("prepare launch session");

        let resource_budget = prepared
            .task
            .resource_budget
            .as_ref()
            .expect("resource budget snapshot");
        assert_eq!(resource_budget.active_session_count, 0);
        assert_eq!(resource_budget.active_install_count, 0);
        assert_eq!(resource_budget.active_memory_allocation_mb, 0);
        assert_eq!(resource_budget.requested_memory_mb, Some(i32::MAX));
        assert!(resource_budget.memory_pressure);
        assert!(!resource_budget.install_pressure);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        for expected in [
            "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.",
            "Guardian Custom mode will keep the selected Java override for this launch.",
            "Switch Guardian back to Managed if you want Croopor to adjust unsafe choices.",
        ] {
            assert!(
                prepared
                    .task
                    .guardian
                    .guidance
                    .iter()
                    .any(|detail| detail == expected),
                "missing guidance: {expected}"
            );
            assert!(
                prepared
                    .task
                    .guardian
                    .details
                    .iter()
                    .any(|detail| detail == expected),
                "missing detail: {expected}"
            );
        }
    }

    #[tokio::test]
    async fn resource_budget_warnings_merge_with_existing_guardian_guidance_before_queue() {
        let fixture = TestFixture::new("prepare-resource-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });
        for index in 0..4 {
            fixture
                .add_active_launch(&format!("active-launch-{index}"), 1024)
                .await;
        }
        fixture.add_active_install("active-install").await;

        let prepared = fixture
            .prepare(instance_id.clone(), Some(i32::MAX))
            .await
            .expect("prepare launch session");

        let resource_budget = prepared
            .task
            .resource_budget
            .as_ref()
            .expect("resource budget snapshot");
        assert_eq!(resource_budget.active_session_count, 4);
        assert_eq!(resource_budget.active_install_count, 1);
        assert_eq!(resource_budget.active_memory_allocation_mb, 4096);
        assert_eq!(resource_budget.requested_memory_mb, Some(i32::MAX));
        assert!(resource_budget.memory_pressure);
        assert!(resource_budget.install_pressure);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        for expected in [
            "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.",
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
            "Active install/download sessions: 1. Launching now can add disk and network pressure during startup.",
            "Guardian Custom mode will keep the selected Java override for this launch.",
        ] {
            assert!(
                prepared
                    .task
                    .guardian
                    .guidance
                    .iter()
                    .any(|detail| detail == expected),
                "missing guidance: {expected}"
            );
        }
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail.starts_with("Launch concurrency may be tight:"))
        );
    }

    #[test]
    fn resource_budget_snapshot_marks_pressure_flags_and_signed_remaining_memory() {
        let pressured = test_budget_with_memory_and_disk(
            LaunchMemoryEvidence {
                host_total_memory_mb: Some(8192),
                host_available_memory_mb: Some(1536),
                host_used_memory_mb: Some(6656),
                launcher_process_memory_mb: Some(128),
            },
            LaunchDiskEvidence {
                launch_disk_available_mb: Some(1024),
            },
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(142),
                host_cpu_load_5m_x100: Some(81),
                host_cpu_load_15m_x100: Some(43),
            },
            Some(4),
            ActiveLaunchResourceUse {
                session_count: 1,
                install_count: 1,
                memory_allocation_mb: 3072,
            },
            4096,
        );

        assert_eq!(pressured.host_total_memory_mb, Some(8192));
        assert_eq!(pressured.host_available_memory_mb, Some(1536));
        assert_eq!(pressured.host_used_memory_mb, Some(6656));
        assert_eq!(pressured.host_cpu_threads, Some(4));
        assert_eq!(pressured.host_cpu_load_1m_x100, Some(142));
        assert_eq!(pressured.host_cpu_load_5m_x100, Some(81));
        assert_eq!(pressured.host_cpu_load_15m_x100, Some(43));
        assert_eq!(pressured.launcher_process_memory_mb, Some(128));
        assert_eq!(pressured.active_session_count, 1);
        assert_eq!(pressured.active_install_count, 1);
        assert_eq!(pressured.active_memory_allocation_mb, 3072);
        assert_eq!(pressured.requested_memory_mb, Some(4096));
        assert_eq!(pressured.estimated_remaining_memory_mb, Some(1024));
        assert_eq!(pressured.memory_headroom_mb, LAUNCH_MEMORY_HEADROOM_MB);
        assert!(pressured.memory_pressure);
        assert!(pressured.cpu_pressure);
        assert!(pressured.install_pressure);
        assert_eq!(pressured.launch_disk_available_mb, Some(1024));
        assert_eq!(pressured.launch_disk_headroom_mb, LAUNCH_DISK_HEADROOM_MB);
        assert!(pressured.disk_pressure);

        let overcommitted = test_budget_with_memory(
            LaunchMemoryEvidence {
                host_total_memory_mb: Some(4096),
                ..LaunchMemoryEvidence::default()
            },
            Some(16),
            0,
            0,
            1024,
            8192,
        );
        assert_eq!(overcommitted.estimated_remaining_memory_mb, Some(-5120));
        assert!(overcommitted.memory_pressure);
        assert!(!overcommitted.cpu_pressure);
        assert!(!overcommitted.install_pressure);
        assert_eq!(overcommitted.launch_disk_available_mb, None);
        assert_eq!(
            overcommitted.launch_disk_headroom_mb,
            LAUNCH_DISK_HEADROOM_MB
        );
        assert!(!overcommitted.disk_pressure);

        let cpu_load_pressured = test_budget_with_memory_and_disk(
            LaunchMemoryEvidence::default(),
            LaunchDiskEvidence::default(),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(1520),
                ..LaunchCpuLoadEvidence::default()
            },
            Some(16),
            ActiveLaunchResourceUse::default(),
            4096,
        );
        assert!(cpu_load_pressured.cpu_pressure);
    }

    #[test]
    fn cpu_load_conversion_is_instant_and_optional() {
        assert_eq!(load_to_x100(0.0), Some(0));
        assert_eq!(load_to_x100(0.424), Some(42));
        assert_eq!(load_to_x100(0.425), Some(43));
        assert_eq!(load_to_x100(12.5), Some(1250));
        assert_eq!(load_to_x100(f64::NAN), None);
        assert_eq!(load_to_x100(f64::INFINITY), None);
        assert_eq!(load_to_x100(-0.1), None);
    }

    struct TestFixture {
        state: AppState,
        paths: AppPaths,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            fs::create_dir_all(&paths.library_dir).expect("create library dir");
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig {
                    library_dir: paths.library_dir.to_string_lossy().to_string(),
                    ..AppConfig::default()
                })
                .expect("set library dir");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, paths, root }
        }

        fn add_instance(&self, name: &str, version_id: &str) -> String {
            self.state
                .instances()
                .add(
                    name.to_string(),
                    version_id.to_string(),
                    String::new(),
                    String::new(),
                    None,
                )
                .expect("add instance")
                .id
        }

        fn write_ready_install(&self, version_id: &str) {
            self.write_version_json(
                version_id,
                serde_json::json!({
                    "id": version_id,
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {},
                    "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                    "libraries": []
                }),
            );
            let version_dir = self.paths.library_dir.join("versions").join(version_id);
            fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
                .expect("write client jar");
            self.write_ready_runtime("java-runtime-delta");
        }

        fn write_child_version(&self, version_id: &str, parent_id: &str) {
            self.write_version_json(
                version_id,
                serde_json::json!({
                    "id": version_id,
                    "inheritsFrom": parent_id,
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {},
                    "libraries": []
                }),
            );
        }

        fn write_ready_runtime(&self, component: &str) {
            let runtime_bin = self
                .paths
                .library_dir
                .join("runtime")
                .join(component)
                .join("bin");
            fs::create_dir_all(&runtime_bin).expect("runtime bin");
            let java_name = if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            };
            fs::write(runtime_bin.join(java_name), b"java").expect("runtime java");
        }

        fn write_global_runtime_without_ready_marker(&self, component: &str) -> PathBuf {
            let runtime_root = self.paths.config_dir.join("runtimes").join(component);
            let runtime_bin = runtime_root.join("bin");
            fs::create_dir_all(&runtime_bin).expect("global runtime bin");
            let java_name = if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            };
            fs::write(runtime_bin.join(java_name), b"java").expect("global runtime java");
            runtime_root
        }

        fn write_manual_java_override(&self) -> String {
            let bin_dir = self.root.join("manual-java").join("bin");
            fs::create_dir_all(&bin_dir).expect("manual java bin");
            let java_path = bin_dir.join(if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            });
            fs::write(&java_path, b"java").expect("manual java");
            java_path.to_string_lossy().to_string()
        }

        fn update_instance(&self, id: &str, update: impl FnOnce(&mut Instance)) {
            let mut instance = self.state.instances().get(id).expect("instance");
            update(&mut instance);
            self.state
                .instances()
                .update(instance)
                .expect("update instance");
        }

        fn set_guardian_mode(&self, mode: &str) {
            let mut config = self.state.config().current();
            config.guardian_mode = mode.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set guardian mode");
        }

        fn set_launch_auth_mode(&self, mode: &str) {
            let mut config = self.state.config().current();
            config.launch_auth_mode = mode.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set launch auth mode");
        }

        fn set_global_jvm_preset(&self, preset: &str) {
            let mut config = self.state.config().current();
            config.jvm_preset = preset.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set global jvm preset");
        }

        fn set_global_java_override(&self, java_path: &str) {
            let mut config = self.state.config().current();
            config.java_path_override = java_path.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set global java override");
        }

        fn write_version_json(&self, version_id: &str, value: serde_json::Value) {
            let version_dir = self.paths.library_dir.join("versions").join(version_id);
            fs::create_dir_all(&version_dir).expect("version dir");
            fs::write(
                version_dir.join(format!("{version_id}.json")),
                serde_json::to_vec(&value).expect("version json"),
            )
            .expect("write version json");
        }

        async fn prepare(
            &self,
            instance_id: String,
            max_memory_mb: Option<i32>,
        ) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
            self.prepare_with_memory(instance_id, max_memory_mb, None)
                .await
        }

        async fn prepare_with_memory(
            &self,
            instance_id: String,
            max_memory_mb: Option<i32>,
            min_memory_mb: Option<i32>,
        ) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
            if let Some(instance) = self.state.instances().get(&instance_id) {
                self.write_ready_install(&instance.version_id);
            }
            prepare_launch_session(
                &self.state,
                LaunchRequest {
                    instance_id,
                    username: None,
                    max_memory_mb,
                    min_memory_mb,
                    client_started_at_ms: None,
                },
            )
            .await
        }

        async fn add_active_launch(&self, session_id: &str, max_memory_mb: u64) {
            self.state
                .sessions()
                .insert(LaunchSessionRecord {
                    session_id: SessionId(session_id.to_string()),
                    instance_id: format!("{session_id}-instance"),
                    version_id: "1.21.1".to_string(),
                    launched_at: Some(timestamp_utc()),
                    benchmark: None,
                    state: LaunchState::Queued,
                    pid: None,
                    process_started_at_ms: None,
                    boot_completed_at_ms: None,
                    boot_duration_ms: None,
                    priority: None,
                    exit_code: None,
                    command: vec!["java".to_string(), format!("-Xmx{max_memory_mb}M")],
                    java_path: None,
                    natives_dir: None,
                    failure: None,
                    healing: None,
                    guardian: None,
                    outcome: None,
                    stages: Vec::new(),
                })
                .await;
        }

        async fn add_active_install(&self, install_id: &str) {
            self.state.installs().insert(install_id.to_string()).await;
        }

        async fn add_active_minecraft_account(&self, owns_minecraft_java: bool) {
            self.state
                .auth_logins()
                .replace_with_msa_and_minecraft_account(
                    NewAuthLoginMsaToken {
                        access_token: "msa-access-token".to_string(),
                        refresh_token: None,
                        id_token: None,
                        token_type: "Bearer".to_string(),
                        expires_in: 3600,
                        scope: Some("XboxLive.signin offline_access".to_string()),
                    },
                    NewAuthLoginMinecraftAccount {
                        access_token: "minecraft-access-token".to_string(),
                        token_type: Some("Bearer".to_string()),
                        expires_in: 86400,
                        profile: AuthLoginMinecraftProfile {
                            id: "4f9c7f7d0b1245d9a5c2f03a8c120001".to_string(),
                            name: "ProfileName".to_string(),
                            skins: Vec::new(),
                            capes: Vec::new(),
                        },
                        owns_minecraft_java,
                    },
                )
                .await;
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-launch-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn assert_readiness_reason(
        preflight: &LaunchPreflightResponse,
        expected: LaunchReadinessReasonId,
    ) {
        assert!(
            preflight
                .readiness
                .reasons
                .iter()
                .any(|reason| reason.id == expected),
            "missing readiness reason {expected:?}: {:?}",
            preflight.readiness.reasons
        );
    }

    fn assert_guardian_fact(preflight: &LaunchPreflightResponse, expected: &str) {
        let _ = guardian_fact(preflight, expected);
    }

    fn guardian_fact<'a>(
        preflight: &'a LaunchPreflightResponse,
        expected: &str,
    ) -> &'a GuardianFact {
        preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == expected)
            .unwrap_or_else(|| {
                panic!(
                    "missing guardian fact {expected}: {:?}",
                    preflight.guardian_facts
                )
            })
    }

    fn readiness_reason(
        preflight: &LaunchPreflightResponse,
        expected: LaunchReadinessReasonId,
    ) -> &LaunchReadinessReason {
        preflight
            .readiness
            .reasons
            .iter()
            .find(|reason| reason.id == expected)
            .unwrap_or_else(|| {
                panic!(
                    "missing readiness reason {expected:?}: {:?}",
                    preflight.readiness.reasons
                )
            })
    }

    fn assert_launch_error_is_token_safe(value: &serde_json::Value) {
        assert_no_sensitive_public_field_keys(value);
        let text = value.to_string();
        for material in [
            "new-msa-access-token",
            "new-msa-refresh-token",
            "old-msa-access-token",
            "old-msa-refresh-token",
            "minecraft-access-token",
            "xbl-token",
            "xsts-token",
            "provider-secret-payload",
        ] {
            assert!(
                !text.contains(material),
                "public launch JSON exposed sensitive material {material}"
            );
        }
    }

    fn assert_no_sensitive_public_field_keys(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    assert!(
                        !matches!(
                            key.as_str(),
                            "access_token" | "refresh_token" | "id_token" | "device_code"
                        ),
                        "public launch JSON exposed {key}"
                    );
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            _ => {}
        }
    }

    fn test_budget_with_memory(
        memory_evidence: LaunchMemoryEvidence,
        host_cpu_threads: Option<usize>,
        active_session_count: usize,
        active_install_count: usize,
        active_memory_allocation_mb: u64,
        requested_memory_mb: i32,
    ) -> LaunchProofResourceBudget {
        test_budget_with_memory_and_disk(
            memory_evidence,
            LaunchDiskEvidence::default(),
            LaunchCpuLoadEvidence::default(),
            host_cpu_threads,
            ActiveLaunchResourceUse {
                session_count: active_session_count,
                install_count: active_install_count,
                memory_allocation_mb: active_memory_allocation_mb,
            },
            requested_memory_mb,
        )
    }

    fn test_budget_with_memory_and_disk(
        memory_evidence: LaunchMemoryEvidence,
        disk_evidence: LaunchDiskEvidence,
        cpu_load_evidence: LaunchCpuLoadEvidence,
        host_cpu_threads: Option<usize>,
        active: ActiveLaunchResourceUse,
        requested_memory_mb: i32,
    ) -> LaunchProofResourceBudget {
        capture_resource_budget_snapshot(
            memory_evidence,
            disk_evidence,
            cpu_load_evidence,
            host_cpu_threads,
            active,
            requested_memory_mb,
        )
    }

    fn assert_has_memory_clamp_warning(guardian: &GuardianSummary) {
        for expected in [
            "Minimum memory was higher than maximum memory, so Croopor clamped the launch minimum to match the maximum allocation.",
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ] {
            assert!(
                guardian.guidance.iter().any(|detail| detail == expected),
                "missing clamp guidance: {expected}"
            );
            assert!(
                guardian.details.iter().any(|detail| detail == expected),
                "missing clamp detail: {expected}"
            );
        }
    }

    fn assert_no_memory_clamp_warning(guardian: &GuardianSummary) {
        for unexpected in [
            "Minimum memory was higher than maximum memory, so Croopor clamped the launch minimum to match the maximum allocation.",
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ] {
            assert!(
                !guardian.guidance.iter().any(|detail| detail == unexpected),
                "unexpected clamp guidance: {unexpected}"
            );
            assert!(
                !guardian.details.iter().any(|detail| detail == unexpected),
                "unexpected clamp detail: {unexpected}"
            );
        }
    }

    fn assert_has_low_memory_allocation_warning(guardian: &GuardianSummary, max_memory_mb: i32) {
        for expected in [
            format!(
                "Launch memory allocation is very low: this instance is limited to less than 2 GB of RAM ({max_memory_mb} MB selected)."
            ),
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.".to_string(),
        ] {
            assert!(
                guardian.guidance.iter().any(|detail| detail == &expected),
                "missing low-memory guidance: {expected}"
            );
            assert!(
                guardian.details.iter().any(|detail| detail == &expected),
                "missing low-memory detail: {expected}"
            );
        }
    }
}
