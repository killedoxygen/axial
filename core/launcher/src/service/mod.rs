use crate::build::{VanillaLaunchPlan, VanillaLaunchRequest, plan_vanilla_launch};
use crate::healing::{HealingEvent, HealingEventKind};
use crate::jvm::{boot_throttle_args, gc_preset_args, resolve_preset};
use crate::runtime::RuntimeSelection;
use crate::types::{LaunchFailureClass, LaunchState};
use croopor_minecraft::{
    JavaRuntimeInfo, JavaVersion, RuntimeEnsureAction, ensure_runtime, resolve_version,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct LaunchIntent {
    pub session_id: String,
    pub library_dir: std::path::PathBuf,
    pub instance_id: String,
    pub version_id: String,
    pub username: String,
    pub requested_java: String,
    pub requested_preset: String,
    pub extra_jvm_args: Vec<String>,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub resolution: Option<(u32, u32)>,
    pub launcher_name: String,
    pub launcher_version: String,
    pub game_dir: Option<std::path::PathBuf>,
    pub advanced_overrides: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AttemptOverrides {
    pub force_managed_runtime: bool,
    pub disable_custom_gc: bool,
    pub preset_override: Option<String>,
    pub fallback_applied: Option<String>,
    pub retry_count: u32,
}

#[derive(Debug, Clone)]
pub struct PreparedLaunchAttempt {
    pub runtime: RuntimeSelection,
    pub effective_preset: String,
    pub plan: VanillaLaunchPlan,
    pub healing: Option<LaunchHealingSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LaunchHealingSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_applied: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced_overrides: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub events: Vec<HealingEvent>,
}

#[derive(Debug, Clone)]
pub struct LaunchPreparationError {
    pub message: String,
    pub failure_class: Option<LaunchFailureClass>,
    pub healing: Option<LaunchHealingSummary>,
}

#[derive(Debug, Clone)]
pub struct RecoveryPlan {
    pub description: String,
    pub action: RecoveryAction,
}

#[derive(Debug, Clone)]
pub enum RecoveryAction {
    DowngradePreset(String),
    DisableCustomGc,
    SwitchManagedRuntime,
}

pub async fn prepare_launch_attempt(
    intent: &LaunchIntent,
    attempt: &AttemptOverrides,
) -> Result<PreparedLaunchAttempt, LaunchPreparationError> {
    let version = resolve_version(&intent.library_dir, &intent.version_id).map_err(|error| {
        LaunchPreparationError {
            message: error.to_string(),
            failure_class: Some(LaunchFailureClass::Unknown),
            healing: None,
        }
    })?;

    let ensured_runtime = ensure_runtime(
        &intent.library_dir,
        &version.java_version,
        &intent.requested_java,
        attempt.force_managed_runtime,
    )
    .await
    .map_err(|error| LaunchPreparationError {
        message: format!("resolve java: {error}"),
        failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
        healing: build_healing_summary(HealingSummaryInput {
            requested_java_path: &intent.requested_java,
            requested_preset: &intent.requested_preset,
            effective_java_path: None,
            effective_preset: None,
            advanced_overrides: intent.advanced_overrides,
            fallback_applied: attempt.fallback_applied.as_deref(),
            retry_count: attempt.retry_count,
            failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
        }),
    })?;

    let mut runtime = runtime_selection_from_ensure(&intent.requested_java, ensured_runtime);
    sanitize_effective_runtime_major(&mut runtime, &version.java_version);

    let loader = infer_loader(&intent.version_id);
    let is_modded = loader != "vanilla" || !version.inherits_from.trim().is_empty();
    let mut effective_preset = attempt.preset_override.clone().unwrap_or_else(|| {
        resolve_preset(
            &intent.requested_preset,
            &intent.version_id,
            loader,
            is_modded,
            &runtime.effective_info,
        )
    });

    if intent.advanced_overrides {
        if let Err((class, message)) = validate_manual_java_override(
            &intent.requested_java,
            &runtime,
            version.java_version.major_version,
        ) {
            return Err(LaunchPreparationError {
                message,
                failure_class: Some(class),
                healing: build_healing_summary(HealingSummaryInput {
                    requested_java_path: &intent.requested_java,
                    requested_preset: &intent.requested_preset,
                    effective_java_path: Some(runtime.effective_path.as_str()),
                    effective_preset: Some(effective_preset.as_str()),
                    advanced_overrides: intent.advanced_overrides,
                    fallback_applied: attempt.fallback_applied.as_deref(),
                    retry_count: attempt.retry_count,
                    failure_class: Some(class),
                }),
            });
        }
        if let Err((class, message)) =
            validate_manual_jvm_args(&intent.extra_jvm_args, &runtime.effective_info)
        {
            return Err(LaunchPreparationError {
                message,
                failure_class: Some(class),
                healing: build_healing_summary(HealingSummaryInput {
                    requested_java_path: &intent.requested_java,
                    requested_preset: &intent.requested_preset,
                    effective_java_path: Some(runtime.effective_path.as_str()),
                    effective_preset: Some(effective_preset.as_str()),
                    advanced_overrides: intent.advanced_overrides,
                    fallback_applied: attempt.fallback_applied.as_deref(),
                    retry_count: attempt.retry_count,
                    failure_class: Some(class),
                }),
            });
        }
    }

    let healing = build_healing_summary(HealingSummaryInput {
        requested_java_path: &intent.requested_java,
        requested_preset: &intent.requested_preset,
        effective_java_path: Some(runtime.effective_path.as_str()),
        effective_preset: Some(effective_preset.as_str()),
        advanced_overrides: intent.advanced_overrides,
        fallback_applied: attempt.fallback_applied.as_deref(),
        retry_count: attempt.retry_count,
        failure_class: None,
    });

    let mut extra_jvm_args = boot_throttle_args(runtime.effective_info.major);
    if !effective_preset.trim().is_empty() && !attempt.disable_custom_gc {
        extra_jvm_args.extend(gc_preset_args(&effective_preset, &runtime.effective_info));
    } else if attempt.disable_custom_gc {
        effective_preset.clear();
    }
    extra_jvm_args.extend(intent.extra_jvm_args.iter().cloned());

    let plan = plan_vanilla_launch(&VanillaLaunchRequest {
        session_id: intent.session_id.clone(),
        mc_dir: intent.library_dir.clone(),
        version_id: intent.version_id.clone(),
        username: intent.username.clone(),
        runtime: runtime.clone(),
        game_dir: intent.game_dir.clone(),
        launcher_name: intent.launcher_name.clone(),
        launcher_version: intent.launcher_version.clone(),
        min_memory_mb: Some(intent.min_memory_mb),
        max_memory_mb: Some(intent.max_memory_mb),
        extra_jvm_args,
        resolution: intent.resolution,
    })
    .map_err(|error| LaunchPreparationError {
        message: error.to_string(),
        failure_class: Some(LaunchFailureClass::Unknown),
        healing: healing.clone(),
    })?;

    Ok(PreparedLaunchAttempt {
        runtime,
        effective_preset,
        plan,
        healing,
    })
}

fn runtime_selection_from_ensure(
    requested_java: &str,
    ensured: croopor_minecraft::RuntimeEnsureResult,
) -> RuntimeSelection {
    let selected = ensured
        .requested
        .clone()
        .unwrap_or_else(|| ensured.effective.clone());
    let selected_path = if requested_java.trim().is_empty() {
        String::new()
    } else {
        selected.java_path.clone()
    };
    let selected_info = if requested_java.trim().is_empty() {
        JavaRuntimeInfo {
            id: String::new(),
            major: 0,
            update: 0,
            distribution: "unknown".to_string(),
            path: String::new(),
        }
    } else {
        selected.info.clone()
    };

    RuntimeSelection {
        requested_path: requested_java.trim().to_string(),
        selected_path,
        selected_info,
        effective_path: ensured.effective.java_path.clone(),
        effective_info: ensured.effective.info.clone(),
        effective_source: ensured.effective.source.as_str().to_string(),
        bypassed_requested_runtime: ensured.bypassed_requested_runtime
            || matches!(ensured.action, RuntimeEnsureAction::BypassRequested),
    }
}

pub fn sanitize_effective_runtime_major(
    runtime: &mut RuntimeSelection,
    java_version: &JavaVersion,
) {
    if runtime.effective_path.is_empty() {
        return;
    }
    if runtime.effective_info.major == 0 && java_version.major_version > 0 {
        runtime.effective_info.major = java_version.major_version as u32;
    }
}

pub struct HealingSummaryInput<'a> {
    pub requested_java_path: &'a str,
    pub requested_preset: &'a str,
    pub effective_java_path: Option<&'a str>,
    pub effective_preset: Option<&'a str>,
    pub advanced_overrides: bool,
    pub fallback_applied: Option<&'a str>,
    pub retry_count: u32,
    pub failure_class: Option<LaunchFailureClass>,
}

pub fn build_healing_summary(input: HealingSummaryInput<'_>) -> Option<LaunchHealingSummary> {
    let requested_java_path = (!input.requested_java_path.trim().is_empty())
        .then(|| input.requested_java_path.to_string());
    let requested_preset = (!input.requested_preset.trim().is_empty())
        .then(|| input.requested_preset.trim().to_string());
    let effective_java_path = input
        .effective_java_path
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let effective_preset = input
        .effective_preset
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let fallback_applied = input
        .fallback_applied
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let failure_class_name = input
        .failure_class
        .map(failure_class_name)
        .map(str::to_string);

    let mut warnings = Vec::new();
    let mut events = Vec::new();

    if let Some(requested) = requested_preset.as_ref() {
        let effective = effective_preset.as_deref().unwrap_or("none");
        if requested != effective {
            let detail = format!(
                "Requested JVM preset \"{requested}\" was downgraded to \"{effective}\" for compatibility"
            );
            warnings.push(detail.clone());
            events.push(HealingEvent {
                kind: HealingEventKind::PresetDowngraded,
                detail: Some(detail),
            });
        }
    }
    if let (Some(requested), Some(effective)) =
        (requested_java_path.as_ref(), effective_java_path.as_ref())
        && requested != effective
    {
        let detail =
            "Requested Java override was bypassed in favor of a safer managed runtime".to_string();
        warnings.push(detail.clone());
        events.push(HealingEvent {
            kind: HealingEventKind::RuntimeBypassed,
            detail: Some(format!("requested={requested} effective={effective}")),
        });
    }
    if let Some(detail) = fallback_applied.as_ref() {
        events.push(HealingEvent {
            kind: HealingEventKind::FallbackApplied,
            detail: Some(detail.clone()),
        });
    }
    if matches!(
        input.failure_class,
        Some(LaunchFailureClass::StartupStalled)
    ) {
        events.push(HealingEvent {
            kind: HealingEventKind::StartupStalled,
            detail: Some("no startup activity observed".to_string()),
        });
    }

    let summary = LaunchHealingSummary {
        requested_preset,
        effective_preset,
        requested_java_path,
        effective_java_path,
        auth_mode: Some("offline".to_string()),
        warnings,
        fallback_applied,
        retry_count: (input.retry_count > 0).then_some(input.retry_count),
        failure_class: failure_class_name,
        advanced_overrides: Some(input.advanced_overrides),
        events,
    };

    if summary.requested_preset.is_none()
        && summary.effective_preset.is_none()
        && summary.requested_java_path.is_none()
        && summary.effective_java_path.is_none()
        && summary.warnings.is_empty()
        && summary.fallback_applied.is_none()
        && summary.retry_count.is_none()
        && summary.failure_class.is_none()
        && summary.events.is_empty()
        && !input.advanced_overrides
    {
        None
    } else {
        Some(summary)
    }
}

pub fn launch_state_name(state: LaunchState) -> &'static str {
    match state {
        LaunchState::Idle => "idle",
        LaunchState::Queued => "queued",
        LaunchState::Planning => "planning",
        LaunchState::Validating => "validating",
        LaunchState::EnsuringRuntime => "ensuring_runtime",
        LaunchState::DownloadingRuntime => "downloading_runtime",
        LaunchState::Preparing => "preparing",
        LaunchState::Starting => "starting",
        LaunchState::Monitoring => "monitoring",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

pub fn is_terminal_status(status: &crate::process::LaunchStatusEvent) -> bool {
    matches!(status.state.as_str(), "failed" | "exited")
}

pub fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

pub fn failure_class_name(class: LaunchFailureClass) -> &'static str {
    match class {
        LaunchFailureClass::Unknown => "unknown",
        LaunchFailureClass::JvmUnsupportedOption => "jvm_unsupported_option",
        LaunchFailureClass::JvmExperimentalUnlock => "jvm_experimental_unlock_required",
        LaunchFailureClass::JvmOptionOrdering => "jvm_option_ordering",
        LaunchFailureClass::JavaRuntimeMismatch => "java_runtime_mismatch",
        LaunchFailureClass::ClasspathModuleConflict => "classpath_or_module_conflict",
        LaunchFailureClass::AuthModeIncompatible => "auth_mode_incompatible",
        LaunchFailureClass::LoaderBootstrapFailure => "loader_bootstrap_failure",
        LaunchFailureClass::StartupStalled => "startup_stalled",
    }
}

pub fn format_failure_class(class: LaunchFailureClass) -> &'static str {
    match class {
        LaunchFailureClass::Unknown => "unknown startup failure",
        LaunchFailureClass::JvmUnsupportedOption => "unsupported JVM option",
        LaunchFailureClass::JvmExperimentalUnlock => "experimental JVM option requires unlock",
        LaunchFailureClass::JvmOptionOrdering => "JVM option ordering conflict",
        LaunchFailureClass::JavaRuntimeMismatch => "Java runtime mismatch",
        LaunchFailureClass::ClasspathModuleConflict => "classpath or module conflict",
        LaunchFailureClass::AuthModeIncompatible => "auth mode incompatibility",
        LaunchFailureClass::LoaderBootstrapFailure => "loader bootstrap failure",
        LaunchFailureClass::StartupStalled => "startup stalled",
    }
}

pub fn infer_loader(version_id: &str) -> &'static str {
    let version = version_id.to_ascii_lowercase();
    if version.contains("neoforge") {
        "neoforge"
    } else if version.contains("fabric") {
        "fabric"
    } else if version.contains("forge") {
        "forge"
    } else if version.contains("quilt") {
        "quilt"
    } else {
        "vanilla"
    }
}

pub fn recovery_for_failure(
    class: LaunchFailureClass,
    version_id: &str,
    info: &JavaRuntimeInfo,
    requested_java: &str,
    advanced_overrides: bool,
    disable_custom_gc: bool,
    effective_preset: &str,
) -> Option<RecoveryPlan> {
    if advanced_overrides {
        return None;
    }

    match class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            if !effective_preset.trim().is_empty() {
                let preset = conservative_healing_preset(version_id, info);
                if !preset.is_empty() && preset != effective_preset {
                    return Some(RecoveryPlan {
                        description: format!(
                            "Automatic retry: downgraded JVM preset to \"{preset}\" after startup failure"
                        ),
                        action: RecoveryAction::DowngradePreset(preset),
                    });
                }
            }
            if !disable_custom_gc {
                return Some(RecoveryPlan {
                    description: "Automatic retry: disabled custom GC flags after startup failure"
                        .to_string(),
                    action: RecoveryAction::DisableCustomGc,
                });
            }
        }
        LaunchFailureClass::JavaRuntimeMismatch => {
            if !requested_java.trim().is_empty() {
                return Some(RecoveryPlan {
                    description: "Automatic retry: switched to managed Java after runtime mismatch"
                        .to_string(),
                    action: RecoveryAction::SwitchManagedRuntime,
                });
            }
        }
        _ => {}
    }
    None
}

pub fn conservative_healing_preset(version_id: &str, info: &JavaRuntimeInfo) -> String {
    if info.major <= 8 || is_legacy_version_family(version_id) {
        "legacy".to_string()
    } else {
        "performance".to_string()
    }
}

fn validate_manual_java_override(
    requested_java: &str,
    runtime: &RuntimeSelection,
    required_major: i32,
) -> Result<(), (LaunchFailureClass, String)> {
    if requested_java.trim().is_empty() || requested_java.trim() != runtime.effective_path.trim() {
        return Ok(());
    }
    if required_major > 0
        && runtime.effective_info.major > 0
        && runtime.effective_info.major as i32 != required_major
    {
        return Err((
            LaunchFailureClass::JavaRuntimeMismatch,
            format!(
                "explicit Java override targets Java {} but this version requires Java {}",
                runtime.effective_info.major, required_major
            ),
        ));
    }
    if required_major == 8
        && runtime.effective_info.major == 8
        && runtime.effective_info.update > 0
        && runtime.effective_info.update < 312
    {
        return Err((
            LaunchFailureClass::JavaRuntimeMismatch,
            format!(
                "explicit Java 8 override is too old for legacy support (8u{} detected; use 8u312 or newer)",
                runtime.effective_info.update
            ),
        ));
    }
    Ok(())
}

fn validate_manual_jvm_args(
    args: &[String],
    info: &JavaRuntimeInfo,
) -> Result<(), (LaunchFailureClass, String)> {
    if args.is_empty() {
        return Ok(());
    }
    let unlock_index = args
        .iter()
        .position(|arg| arg == "-XX:+UnlockExperimentalVMOptions");
    for (index, arg) in args.iter().enumerate() {
        match () {
            _ if arg == "-XX:+UseShenandoahGC" && !crate::jvm::supports_shenandoah(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request Shenandoah on an unsupported runtime".to_string(),
                ));
            }
            _ if arg == "-XX:+UseZGC" && !crate::jvm::supports_zgc(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request ZGC on an unsupported runtime".to_string(),
                ));
            }
            _ if arg == "-XX:+ZGenerational" && !crate::jvm::supports_generational_zgc(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request Generational ZGC on an unsupported runtime"
                        .to_string(),
                ));
            }
            _ if arg.starts_with("-XX:G1NewSizePercent=")
                || arg.starts_with("-XX:G1MaxNewSizePercent=") =>
            {
                if !crate::jvm::supports_hotspot_tuning(info) {
                    return Err((
                        LaunchFailureClass::JvmUnsupportedOption,
                        "explicit JVM args request experimental G1 tuning on an unsupported runtime"
                            .to_string(),
                    ));
                }
                if unlock_index.is_none() {
                    return Err((
                        LaunchFailureClass::JvmExperimentalUnlock,
                        "explicit JVM args require -XX:+UnlockExperimentalVMOptions".to_string(),
                    ));
                }
                if unlock_index.is_some_and(|unlock| unlock > index) {
                    return Err((
                        LaunchFailureClass::JvmOptionOrdering,
                        "explicit JVM args place -XX:+UnlockExperimentalVMOptions after dependent flags"
                            .to_string(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_legacy_version_family(version_id: &str) -> bool {
    let base = version_id.split("-forge-").next().unwrap_or(version_id);
    let numbers = base
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    matches!(numbers.as_slice(), [1, minor, ..] if *minor <= 12)
}

pub fn snapshot_status(
    record: &crate::process::LaunchSessionRecord,
) -> crate::process::LaunchStatusEvent {
    crate::process::LaunchStatusEvent {
        state: launch_state_name(record.state).to_string(),
        pid: record.pid,
        exit_code: record.exit_code,
        failure_class: record
            .failure
            .as_ref()
            .map(|failure| failure_class_name(failure.class).to_string()),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.clone()),
        healing: record.healing.clone(),
    }
}

pub fn maybe_classify_stalled(record: &crate::process::LaunchSessionRecord) -> Option<String> {
    record
        .failure
        .as_ref()
        .map(|failure| failure_class_name(failure.class).to_string())
}

pub fn library_dir_not_configured(path: &Path) -> bool {
    path.as_os_str().is_empty()
}
