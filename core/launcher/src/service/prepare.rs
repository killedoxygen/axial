use super::{
    AttemptOverrides, HealingSummaryInput, LaunchIntent, LaunchPreparationError,
    LaunchPreparationMetrics, PreparedLaunchAttempt, build_healing_summary, infer_loader,
};
use crate::build::{VanillaLaunchRequest, plan_resolved_launch};
use crate::jvm::{boot_throttle_args, gc_preset_args, resolve_preset};
use crate::runtime::RuntimeSelection;
use crate::types::LaunchFailureClass;
use croopor_minecraft::{
    JavaRuntimeInfo, JavaVersion, RuntimeEnsureAction, ensure_runtime, resolve_version,
};
use std::time::Instant;

pub async fn prepare_launch_attempt(
    intent: &LaunchIntent,
    attempt: &AttemptOverrides,
) -> Result<PreparedLaunchAttempt, LaunchPreparationError> {
    let started_at = Instant::now();
    let version_started_at = Instant::now();
    let version = resolve_version(&intent.library_dir, &intent.version_id).map_err(|error| {
        LaunchPreparationError {
            message: error.to_string(),
            failure_class: Some(LaunchFailureClass::Unknown),
            healing: None,
        }
    })?;
    let version_ms = version_started_at.elapsed().as_millis();

    let runtime_started_at = Instant::now();
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
    let runtime_ms = runtime_started_at.elapsed().as_millis();

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
        if let Err((class, message)) = super::validation::validate_manual_java_override(
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
        if let Err((class, message)) = super::validation::validate_manual_jvm_args(
            &intent.extra_jvm_args,
            &runtime.effective_info,
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
        extra_jvm_args.extend(gc_preset_args(
            &effective_preset,
            &runtime.effective_info,
            uses_low_impact_startup(&intent.performance_mode),
        ));
    } else if attempt.disable_custom_gc {
        effective_preset.clear();
    }
    extra_jvm_args.extend(intent.extra_jvm_args.iter().cloned());

    let planning_started_at = Instant::now();
    let plan = plan_resolved_launch(
        &VanillaLaunchRequest {
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
        },
        version,
    )
    .map_err(|error| LaunchPreparationError {
        message: error.to_string(),
        failure_class: Some(LaunchFailureClass::Unknown),
        healing: healing.clone(),
    })?;
    let planning_ms = planning_started_at.elapsed().as_millis();

    Ok(PreparedLaunchAttempt {
        runtime,
        effective_preset,
        plan,
        healing,
        metrics: LaunchPreparationMetrics {
            version_ms,
            runtime_ms,
            planning_ms,
            total_ms: started_at.elapsed().as_millis(),
        },
    })
}

fn uses_low_impact_startup(performance_mode: &str) -> bool {
    !matches!(performance_mode.trim(), "custom")
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
