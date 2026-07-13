use super::{optional_value, required_value};
use crate::guardian::{
    GuardianFact, performance_health_guardian_facts, performance_plan_guardian_facts,
    performance_state_error_guardian_fact,
};
use crate::observability::{PerformanceProofRecord, performance_health_proof_record};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass as StateOwnershipClass, RollbackState,
    StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::{
    AppState, InstalledVersionsSnapshot, ManagedInspectionError, ManagedInstanceAdmissionError,
    ProducerLease,
};
use axial_performance::{
    BundleHealth, CompositionPlan, CompositionTier, InstallError, ManagedArtifactProvider,
    ManagedMutationError, OwnershipClass, PerformanceMode, ResolutionRequest, StateError,
    effective_performance_plan, parse_mode,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

const PERFORMANCE_MANAGED_ARTIFACT_SUMMARY_LIMIT: usize = 50;
pub(super) const PERFORMANCE_DATA_INTERNAL_ERROR: &str =
    "Could not load performance data. Check app data permissions and try again.";

#[derive(Debug, Deserialize)]
pub struct PerformancePlanRequest {
    pub game_version: Option<String>,
    pub loader: Option<String>,
    pub mode: Option<String>,
    pub instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PerformanceHealthRequest {
    pub instance_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PerformancePlanResponse {
    pub active: bool,
    pub effective: axial_performance::EffectivePerformancePlan,
    pub guardian_facts: Vec<GuardianFact>,
    #[serde(flatten)]
    pub plan: CompositionPlan,
}

#[derive(Debug, Serialize)]
pub struct PerformanceHealthResponse {
    pub active: bool,
    pub health: BundleHealth,
    pub composition_id: String,
    pub tier: String,
    pub installed_count: usize,
    pub managed_artifacts: Vec<PerformanceManagedArtifactSummary>,
    pub warnings: Vec<String>,
    pub guardian_facts: Vec<GuardianFact>,
    pub proof: PerformanceProofRecord,
    pub view_model: super::super::PerformancePlanSummaryViewModel,
    pub display: PerformanceInstanceDisplay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PerformanceManagedArtifactSummary {
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub ownership_class: OwnershipClass,
    pub source_provider: ManagedArtifactProvider,
    pub sha512_present: bool,
    pub sha512_verified: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceInstanceDisplay {
    pub memory: PerformanceMemoryDisplay,
    pub runtime: PerformanceRuntimeDisplay,
    pub mode: PerformanceModeDisplay,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceMemoryDisplay {
    pub min_gb: f32,
    pub max_gb: f32,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceRuntimeDisplay {
    pub detected: bool,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceModeDisplay {
    pub mode: String,
    pub label: String,
    pub source: String,
    pub source_label: String,
}

pub async fn performance_plan(
    state: &AppState,
    query: PerformancePlanRequest,
) -> Result<PerformancePlanResponse, (StatusCode, Json<serde_json::Value>)> {
    let game_version = required_value(
        query.game_version.as_deref(),
        "game_version query parameter is required",
    )?;
    let mode = resolve_config_mode(state, query.mode.as_deref())?;
    let request = ResolutionRequest {
        game_version,
        loader: optional_value(query.loader.as_deref()).unwrap_or_default(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: Vec::new(),
    };
    let plan = match optional_value(query.instance_id.as_deref()) {
        Some(instance_id) => {
            state
                .resolve_managed_instance(&instance_id, request)
                .await
                .map_err(managed_inspection_error)?
                .plan
        }
        None => state.performance().get_plan(request),
    };

    let guardian_facts = performance_plan_guardian_facts(&plan, OperationPhase::Planning);

    Ok(PerformancePlanResponse {
        active: matches!(mode, PerformanceMode::Managed),
        effective: effective_performance_plan(&plan),
        guardian_facts,
        plan,
    })
}

pub(crate) async fn performance_health(
    state: &AppState,
    query: PerformanceHealthRequest,
    producer: &ProducerLease,
) -> Result<PerformanceHealthResponse, (StatusCode, Json<serde_json::Value>)> {
    let instance_id = required_value(
        query.instance_id.as_deref(),
        "instance_id query parameter is required",
    )?;
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mode = resolve_instance_mode(state, &instance, None)?;
    let installed_versions = state
        .installed_versions_snapshot(producer)
        .await
        .map(|lookup| lookup.snapshot);
    let display = performance_instance_display(state, &instance, mode, installed_versions.as_ref());

    if !matches!(mode, PerformanceMode::Managed) {
        return Ok(disabled_health_response(mode, display));
    }

    if let Err(error) = state.inspect_managed_instance(&instance.id, None).await {
        if let Some((warning, guardian_facts)) = managed_state_invalid_response(&error) {
            return Ok(invalid_managed_health_response(
                warning,
                guardian_facts,
                display,
            ));
        }
        return Err(managed_inspection_error(error));
    }

    let (game_version, loader) =
        resolve_instance_version_target(installed_versions.as_ref(), &instance, None, None)?;
    let resolved = state
        .resolve_managed_instance(
            &instance.id,
            ResolutionRequest {
                game_version,
                loader,
                mode,
                hardware: state.performance().hardware(),
                installed_mods: Vec::new(),
            },
        )
        .await
        .map_err(managed_inspection_error)?;
    let plan = resolved.plan;
    let inspection = resolved.inspection;
    let health = inspection.health;
    let warnings = response_warnings(&plan, inspection.warnings);
    let state_file = inspection.state;
    let composition_id = state_file
        .as_ref()
        .map(|value| value.composition_id.clone())
        .unwrap_or_default();
    let tier = state_file
        .as_ref()
        .map(|value| tier_name(value.tier).to_string())
        .unwrap_or_default();
    let installed_count = state_file
        .as_ref()
        .map(|value| value.installed_mods.len())
        .unwrap_or_default();
    let guardian_facts = performance_health_guardian_facts(
        health,
        &composition_id,
        &warnings,
        OperationPhase::Validating,
    );
    let rollback = if inspection.rollback_snapshots.is_empty() {
        RollbackState::Unavailable
    } else {
        RollbackState::Available
    };
    let proof = performance_health_proof(
        None,
        health,
        &composition_id,
        &tier,
        installed_count,
        warnings.len(),
        rollback,
    );
    let view_model = super::super::performance_plan_summary_view_model(
        mode,
        Some(&plan),
        health,
        state_file.as_ref().map(|value| value.tier),
        rollback,
        installed_count,
        &warnings,
    );
    let public_composition_id =
        super::super::public_performance_descriptor(&composition_id, "composition");

    Ok(PerformanceHealthResponse {
        active: true,
        health,
        composition_id: public_composition_id,
        tier,
        installed_count,
        managed_artifacts: managed_artifact_summary(state_file.as_ref()),
        warnings,
        guardian_facts,
        proof,
        view_model,
        display,
    })
}

pub(super) fn managed_artifact_summary(
    state: Option<&axial_performance::CompositionState>,
) -> Vec<PerformanceManagedArtifactSummary> {
    state
        .map(|state| {
            state
                .installed_mods
                .iter()
                .take(PERFORMANCE_MANAGED_ARTIFACT_SUMMARY_LIMIT)
                .map(|installed| PerformanceManagedArtifactSummary {
                    project_id: installed.project_id.clone(),
                    version_id: installed.version_id.clone(),
                    filename: installed.filename.clone(),
                    ownership_class: installed.ownership_class,
                    source_provider: installed.source.provider,
                    sha512_present: !installed.integrity.sha512.trim().is_empty(),
                    sha512_verified: installed.integrity.sha512_verified,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn performance_health_proof(
    operation_id: Option<OperationId>,
    health: BundleHealth,
    composition_id: &str,
    tier: &str,
    installed_count: usize,
    warning_count: usize,
    rollback: RollbackState,
) -> PerformanceProofRecord {
    performance_health_proof_record(
        operation_id,
        performance_composition_target(composition_id),
        bundle_health_token(health),
        rollback,
        vec![
            ("composition_id", proof_token(composition_id, "none")),
            ("tier", proof_token(tier, "none")),
            ("managed_artifact_count", installed_count.to_string()),
            ("warning_count", warning_count.to_string()),
        ],
    )
}

pub(super) fn bundle_health_token(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "healthy",
        BundleHealth::Degraded => "degraded",
        BundleHealth::Fallback => "fallback",
        BundleHealth::Disabled => "disabled",
        BundleHealth::Invalid => "invalid",
    }
}

fn proof_token(value: &str, fallback: &str) -> String {
    super::super::public_performance_descriptor(value, fallback)
}

pub(super) fn performance_composition_target(composition_id: &str) -> TargetDescriptor {
    let id = super::super::public_performance_descriptor(composition_id, "performance_composition");
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::PerformanceComposition,
        id,
        StateOwnershipClass::CompositionManaged,
    )
}

pub(super) fn performance_artifacts_target(composition_id: &str) -> TargetDescriptor {
    let id = super::super::public_performance_descriptor(composition_id, "performance_composition");
    let target_id = if id == "performance_composition" {
        "managed_performance_artifacts".to_string()
    } else {
        format!("{id}_managed_artifacts")
    };
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::Artifact,
        target_id,
        StateOwnershipClass::CompositionManaged,
    )
}

fn performance_instance_display(
    state: &AppState,
    instance: &axial_config::Instance,
    mode: PerformanceMode,
    installed_versions: Option<&InstalledVersionsSnapshot>,
) -> PerformanceInstanceDisplay {
    let config = state.config().current();
    let min_gb = memory_gb(instance.min_memory_mb, config.min_memory_mb, 1024);
    let max_gb = memory_gb(instance.max_memory_mb, config.max_memory_mb, 4096);
    let java_major = instance_java_major(installed_versions, &instance.version_id);
    let mode_source = if parse_mode(&instance.performance_mode).is_some() {
        ("instance", "Per instance")
    } else {
        ("global", "Global default")
    };

    PerformanceInstanceDisplay {
        memory: PerformanceMemoryDisplay {
            min_gb,
            max_gb,
            label: heap_label(min_gb, max_gb),
        },
        runtime: PerformanceRuntimeDisplay {
            detected: java_major.is_some(),
            label: java_major
                .map(|major| format!("Java {major}"))
                .unwrap_or_else(|| "Managed Java".to_string()),
        },
        mode: PerformanceModeDisplay {
            mode: performance_mode_token(mode).to_string(),
            label: performance_mode_label(mode).to_string(),
            source: mode_source.0.to_string(),
            source_label: mode_source.1.to_string(),
        },
    }
}

fn instance_java_major(
    installed_versions: Option<&InstalledVersionsSnapshot>,
    version_id: &str,
) -> Option<i32> {
    installed_versions
        .and_then(|snapshot| {
            snapshot
                .report()
                .versions
                .iter()
                .find(|version| version.id == version_id)
        })
        .and_then(|version| (version.java_major > 0).then_some(version.java_major))
}

fn memory_gb(instance_mb: i32, config_mb: i32, fallback_mb: i32) -> f32 {
    let mb = if instance_mb > 0 {
        instance_mb
    } else if config_mb > 0 {
        config_mb
    } else {
        fallback_mb
    };
    mb as f32 / 1024.0
}

fn heap_label(min_gb: f32, max_gb: f32) -> String {
    if (min_gb - max_gb).abs() < f32::EPSILON {
        format!("{} GB", fmt_heap_gb(max_gb))
    } else {
        format!("{} to {} GB", fmt_heap_gb(min_gb), fmt_heap_gb(max_gb))
    }
}

fn fmt_heap_gb(gb: f32) -> String {
    if (gb.fract()).abs() < f32::EPSILON {
        format!("{}", gb as i32)
    } else {
        format!("{gb:.1}")
    }
}

fn performance_mode_label(mode: PerformanceMode) -> &'static str {
    match mode {
        PerformanceMode::Managed => "Managed",
        PerformanceMode::Vanilla => "Vanilla",
        PerformanceMode::Custom => "Custom",
    }
}

fn performance_mode_token(mode: PerformanceMode) -> &'static str {
    match mode {
        PerformanceMode::Managed => "managed",
        PerformanceMode::Vanilla => "vanilla",
        PerformanceMode::Custom => "custom",
    }
}

fn disabled_health_response(
    mode: PerformanceMode,
    display: PerformanceInstanceDisplay,
) -> PerformanceHealthResponse {
    PerformanceHealthResponse {
        active: false,
        health: BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: Vec::new(),
        guardian_facts: Vec::new(),
        proof: performance_health_proof(
            None,
            BundleHealth::Disabled,
            "",
            "",
            0,
            0,
            RollbackState::NotApplicable,
        ),
        view_model: super::super::performance_plan_summary_view_model(
            mode,
            None,
            BundleHealth::Disabled,
            None,
            RollbackState::NotApplicable,
            0,
            &[],
        ),
        display,
    }
}

fn managed_state_invalid_response(
    error: &ManagedInspectionError,
) -> Option<(&'static str, Vec<GuardianFact>)> {
    let ManagedInspectionError::Operation(ManagedMutationError::Definite(InstallError::State(
        state_error,
    ))) = error
    else {
        return None;
    };
    match state_error {
        StateError::Parse(_) => Some(("failed to parse performance state", Vec::new())),
        StateError::InvalidOwnership { .. } => Some((
            "invalid performance artifact ownership metadata",
            performance_state_error_guardian_fact(state_error, OperationPhase::Validating)
                .into_iter()
                .collect(),
        )),
        StateError::InvalidIntegrity { .. } => Some((
            "invalid performance artifact integrity metadata",
            Vec::new(),
        )),
        _ => None,
    }
}

fn invalid_managed_health_response(
    warning: &'static str,
    guardian_facts: Vec<GuardianFact>,
    display: PerformanceInstanceDisplay,
) -> PerformanceHealthResponse {
    let warnings = vec![warning.to_string()];
    PerformanceHealthResponse {
        active: true,
        health: BundleHealth::Invalid,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: warnings.clone(),
        guardian_facts,
        proof: performance_health_proof(
            None,
            BundleHealth::Invalid,
            "",
            "",
            0,
            1,
            RollbackState::Unavailable,
        ),
        view_model: super::super::performance_plan_summary_view_model(
            PerformanceMode::Managed,
            None,
            BundleHealth::Invalid,
            None,
            RollbackState::Unavailable,
            0,
            &warnings,
        ),
        display,
    }
}

fn managed_inspection_error(
    error: ManagedInspectionError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        ManagedInspectionError::Admission(ManagedInstanceAdmissionError::InstanceNotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        ),
        ManagedInspectionError::Admission(
            ManagedInstanceAdmissionError::InvalidInstanceIdentity,
        ) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "instance identity is invalid" })),
        ),
        ManagedInspectionError::Admission(ManagedInstanceAdmissionError::ActiveSession) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "managed composition mutation is blocked while the instance is running"
            })),
        ),
        ManagedInspectionError::Admission(
            error @ (ManagedInstanceAdmissionError::ForeignForegroundAuthority
            | ManagedInstanceAdmissionError::Owner(_)),
        ) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error.to_string() })),
        ),
        ManagedInspectionError::Operation(ManagedMutationError::Definite(InstallError::State(
            StateError::Parse(_),
        ))) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "failed to parse performance state" })),
        ),
        ManagedInspectionError::Operation(ManagedMutationError::Definite(InstallError::State(
            StateError::InvalidOwnership { .. },
        ))) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact ownership metadata"
            })),
        ),
        ManagedInspectionError::Operation(ManagedMutationError::Definite(InstallError::State(
            StateError::InvalidIntegrity { .. },
        ))) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact integrity metadata"
            })),
        ),
        error => internal_error(error),
    }
}

pub(super) fn resolve_instance_version_target(
    installed_versions: Option<&InstalledVersionsSnapshot>,
    instance: &axial_config::Instance,
    game_version_override: Option<&str>,
    loader_override: Option<&str>,
) -> Result<(String, String), (StatusCode, Json<serde_json::Value>)> {
    let explicit_game_version = optional_value(game_version_override);
    let explicit_loader = optional_value(loader_override);
    if let Some(game_version) = explicit_game_version.clone()
        && let Some(loader) = explicit_loader.clone()
    {
        return Ok((game_version, loader));
    }

    let installed_versions = installed_versions.ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        )
    })?;
    let version = installed_versions
        .report()
        .versions
        .iter()
        .find(|version| version.id == instance.version_id)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "instance version metadata is unavailable; install the version before resolving performance files"
                })),
            )
        })?;

    let game_version = explicit_game_version.unwrap_or_else(|| {
        let parent = version.inherits_from.trim();
        if parent.is_empty() {
            version.id.clone()
        } else {
            parent.to_string()
        }
    });
    let loader = explicit_loader.unwrap_or_else(|| {
        version
            .loader
            .as_ref()
            .map(|loader| loader.component_id.short_key().to_string())
            .unwrap_or_else(|| "vanilla".to_string())
    });

    Ok((game_version, loader))
}

pub(super) fn tier_name(tier: CompositionTier) -> &'static str {
    match tier {
        CompositionTier::Extended => "extended",
        CompositionTier::Core => "core",
        CompositionTier::VanillaEnhanced => "vanilla_enhanced",
    }
}

fn resolve_config_mode(
    state: &AppState,
    raw: Option<&str>,
) -> Result<PerformanceMode, (StatusCode, Json<serde_json::Value>)> {
    if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        return parse_mode(raw).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid performance mode" })),
            )
        });
    }
    Ok(parse_mode(&state.config().current().performance_mode).unwrap_or(PerformanceMode::Managed))
}

pub(super) fn resolve_instance_mode(
    state: &AppState,
    instance: &axial_config::Instance,
    raw: Option<&str>,
) -> Result<PerformanceMode, (StatusCode, Json<serde_json::Value>)> {
    if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        return parse_mode(raw).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid performance mode" })),
            )
        });
    }
    if let Some(mode) = parse_mode(&instance.performance_mode) {
        return Ok(mode);
    }
    resolve_config_mode(state, None)
}

pub(super) fn response_warnings(
    plan: &CompositionPlan,
    health_warnings: Vec<String>,
) -> Vec<String> {
    let mut warnings = plan.warnings.clone();
    warnings.extend(health_warnings);
    warnings
}

pub(super) fn internal_error(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": PERFORMANCE_DATA_INTERNAL_ERROR })),
    )
}
