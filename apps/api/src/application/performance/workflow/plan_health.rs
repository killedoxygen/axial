use super::{optional_value, required_value};
use crate::guardian::{
    GuardianFact, performance_failure_memory_guardian_fact, performance_health_guardian_facts,
    performance_plan_guardian_facts, performance_state_error_guardian_fact,
};
use crate::observability::{PerformanceProofRecord, performance_health_proof_record};
use crate::state::AppState;
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass as StateOwnershipClass, RollbackState,
    StabilizationSystem, TargetDescriptor, TargetKind,
};
use axum::{Json, http::StatusCode};
use croopor_minecraft::scan_versions;
use croopor_performance::{
    BundleHealth, CompositionPlan, CompositionTier, ManagedArtifactProvider, OwnershipClass,
    PerformanceMode, ResolutionRequest, StateError, derive_health, effective_performance_plan,
    load_state, parse_mode,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const PERFORMANCE_MANAGED_ARTIFACT_SUMMARY_LIMIT: usize = 50;
const PERFORMANCE_GUARDIAN_FACT_LIMIT: usize = 16;
pub(super) const PERFORMANCE_DATA_INTERNAL_ERROR: &str =
    "Could not load performance data. Check app data permissions and try again.";
pub(super) const PERFORMANCE_STATE_PARSE_WARNING: &str = "failed to parse performance state";

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
    pub effective: croopor_performance::EffectivePerformancePlan,
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
    let installed_mods = plan_installed_mod_evidence(state, query.instance_id.as_deref())?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader: optional_value(query.loader.as_deref()).unwrap_or_default(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods,
    });

    let mut guardian_facts = performance_plan_guardian_facts(&plan, OperationPhase::Planning);
    append_performance_guardian_facts(
        &mut guardian_facts,
        performance_failure_memory_facts(
            state,
            OperationPhase::Planning,
            Some(&plan.composition_id),
        ),
    );

    Ok(PerformancePlanResponse {
        active: matches!(mode, PerformanceMode::Managed),
        effective: effective_performance_plan(&plan),
        guardian_facts,
        plan,
    })
}

fn plan_installed_mod_evidence(
    state: &AppState,
    raw_instance_id: Option<&str>,
) -> Result<Vec<String>, (StatusCode, Json<serde_json::Value>)> {
    let Some(instance_id) = optional_value(raw_instance_id) else {
        return Ok(Vec::new());
    };
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let state_file = match load_state(&mods_dir) {
        Ok(state_file) => state_file,
        Err(StateError::Parse(_)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "failed to parse performance state" })),
            ));
        }
        Err(StateError::InvalidOwnership { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid performance artifact ownership metadata"
                })),
            ));
        }
        Err(StateError::InvalidIntegrity { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid performance artifact integrity metadata"
                })),
            ));
        }
        Err(error) => return Err(internal_error(error)),
    };

    Ok(installed_mod_evidence(&mods_dir, state_file.as_ref()))
}

pub async fn performance_health(
    state: &AppState,
    query: PerformanceHealthRequest,
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
    let display = performance_instance_display(state, &instance, mode);

    if !matches!(mode, PerformanceMode::Managed) {
        return Ok(disabled_health_response(mode, display));
    }

    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let state_file = match load_state(&mods_dir) {
        Ok(state_file) => state_file,
        Err(StateError::Parse(_)) => {
            return Ok(invalid_health_response(
                PERFORMANCE_STATE_PARSE_WARNING,
                Vec::new(),
                display,
            ));
        }
        Err(error @ StateError::InvalidOwnership { .. }) => {
            return Ok(invalid_health_response(
                "invalid performance artifact ownership metadata",
                performance_state_error_guardian_fact(&error, OperationPhase::Validating)
                    .into_iter()
                    .collect(),
                display,
            ));
        }
        Err(StateError::InvalidIntegrity { .. }) => {
            return Ok(invalid_health_response(
                "invalid performance artifact integrity metadata",
                Vec::new(),
                display,
            ));
        }
        Err(error) => return Err(internal_error(error)),
    };
    let (game_version, loader) = resolve_instance_version_target(state, &instance, None, None)?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader,
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_evidence(&mods_dir, state_file.as_ref()),
    });
    let (health, warnings) = derive_health(state_file.as_ref(), Some(&plan), &mods_dir);
    let warnings = response_warnings(&plan, warnings);
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
    let mut guardian_facts = guardian_facts;
    append_performance_guardian_facts(
        &mut guardian_facts,
        performance_failure_memory_facts(state, OperationPhase::Validating, Some(&composition_id)),
    );
    let rollback = health_rollback_state(state, &mods_dir);
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
    state: Option<&croopor_performance::CompositionState>,
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

fn performance_failure_memory_facts(
    state: &AppState,
    phase: OperationPhase,
    target_id: Option<&str>,
) -> Vec<GuardianFact> {
    let target_id = target_id.map(str::trim).filter(|value| !value.is_empty());
    state
        .failure_memory()
        .list()
        .into_iter()
        .filter(|entry| match target_id {
            Some(target_id) => entry.target.id == target_id,
            None => true,
        })
        .filter_map(|entry| performance_failure_memory_guardian_fact(&entry, phase))
        .take(PERFORMANCE_GUARDIAN_FACT_LIMIT)
        .collect()
}

fn append_performance_guardian_facts(facts: &mut Vec<GuardianFact>, more: Vec<GuardianFact>) {
    let remaining = PERFORMANCE_GUARDIAN_FACT_LIMIT.saturating_sub(facts.len());
    facts.extend(more.into_iter().take(remaining));
    facts.truncate(PERFORMANCE_GUARDIAN_FACT_LIMIT);
}

fn health_rollback_state(state: &AppState, mods_dir: &std::path::Path) -> RollbackState {
    match state.performance().list_rollback_snapshots(mods_dir) {
        Ok(snapshots) if !snapshots.is_empty() => RollbackState::Available,
        _ => RollbackState::Unavailable,
    }
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
    instance: &croopor_config::Instance,
    mode: PerformanceMode,
) -> PerformanceInstanceDisplay {
    let config = state.config().current();
    let min_gb = memory_gb(instance.min_memory_mb, config.min_memory_mb, 1024);
    let max_gb = memory_gb(instance.max_memory_mb, config.max_memory_mb, 4096);
    let java_major = instance_java_major(state, &instance.version_id);
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

fn instance_java_major(state: &AppState, version_id: &str) -> Option<i32> {
    state
        .library_dir()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .and_then(|path| scan_versions(&path).ok())
        .and_then(|versions| {
            versions
                .into_iter()
                .find(|version| version.id == version_id)
                .and_then(|version| (version.java_major > 0).then_some(version.java_major))
        })
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

pub(super) fn invalid_health_response(
    warning: impl Into<String>,
    guardian_facts: Vec<GuardianFact>,
    display: PerformanceInstanceDisplay,
) -> PerformanceHealthResponse {
    let warning = warning.into();
    let warnings = vec![warning];
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

pub(super) fn resolve_instance_version_target(
    state: &AppState,
    instance: &croopor_config::Instance,
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

    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let versions = scan_versions(&std::path::PathBuf::from(library_dir)).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "Could not scan installed versions. Check the library folder and try again."
            })),
        )
    })?;
    let version = versions
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
    instance: &croopor_config::Instance,
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

pub(super) fn installed_mod_evidence(
    mods_dir: &std::path::Path,
    state: Option<&croopor_performance::CompositionState>,
) -> Vec<String> {
    let mut evidence = std::collections::BTreeSet::new();
    if let Some(state) = state {
        for installed in &state.installed_mods {
            add_mod_evidence(&mut evidence, &installed.project_id);
        }
    }
    for value in installed_mod_file_evidence(mods_dir) {
        evidence.insert(value);
    }
    evidence.into_iter().collect()
}

pub(super) fn installed_mod_evidence_from_mods_dir(mods_dir: &std::path::Path) -> Vec<String> {
    let state = load_state(mods_dir).ok().flatten();
    installed_mod_evidence(mods_dir, state.as_ref())
}

fn installed_mod_file_evidence(mods_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(mods_dir) else {
        return Vec::new();
    };
    let mut evidence = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("jar"))
        {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            add_mod_evidence(&mut evidence, stem);
        }
    }
    evidence.into_iter().collect()
}

fn add_mod_evidence(evidence: &mut std::collections::BTreeSet<String>, raw: &str) {
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return;
    }
    evidence.insert(normalized.clone());

    let mut prefix = String::new();
    for token in normalized
        .split(|value: char| !value.is_ascii_alphanumeric())
        .filter(|value| !value.is_empty())
    {
        if is_versionish_mod_filename_token(token) {
            break;
        }
        if prefix.is_empty() {
            prefix.push_str(token);
        } else {
            prefix.push('-');
            prefix.push_str(token);
        }
        evidence.insert(prefix.clone());
    }
}

fn is_versionish_mod_filename_token(token: &str) -> bool {
    token.strip_prefix("mc").is_some_and(|suffix| {
        suffix
            .as_bytes()
            .first()
            .is_some_and(|value| value.is_ascii_digit())
    }) || token.strip_prefix('v').is_some_and(|suffix| {
        suffix
            .as_bytes()
            .first()
            .is_some_and(|value| value.is_ascii_digit())
    }) || token
        .as_bytes()
        .first()
        .is_some_and(|value| value.is_ascii_digit())
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
