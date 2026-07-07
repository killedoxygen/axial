use super::readiness::readiness_has_managed_runtime_missing;
use super::{LaunchPreflightFacts, application_guardian_mode, build_launch_preflight_facts};
use crate::application::launch::policy;
use crate::application::{LaunchBoundaryStagingRequest, stage_launch_boundary};
use crate::execution::runtime::{
    ManagedRuntimeRoot, ManagedRuntimeVerificationRequest, verify_managed_runtime,
};
use crate::guardian::{
    GuardianDecisionKind as ApiGuardianDecisionKind, GuardianManagedRuntimeRepairRequest,
    GuardianPreflightOutcome, GuardianRepairPlanningContext, GuardianRepairStatus,
    GuardianUserOutcome, execute_managed_runtime_ready_marker_repair, guardian_fact_from_execution,
    plan_managed_runtime_ready_marker_repair, runtime_repair_user_outcome,
};
use crate::logging::timestamp_utc;
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
};
use crate::state::AppState;
use crate::state::contracts::OperationPhase;
use croopor_config::{AppPaths, Instance};
use croopor_launcher::{GuardianDecision, GuardianMode, GuardianSummary};
use croopor_minecraft::{
    managed_runtime_contents_verified_without_probe, preferred_runtime_component, resolve_version,
};
use std::path::{Path, PathBuf};

pub(super) async fn maybe_repair_managed_runtime_before_launch(
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
    let Ok(repair_plan) = plan_managed_runtime_ready_marker_repair(
        &repair_boundary.guardian_decision,
        GuardianRepairPlanningContext::current_operation(),
    ) else {
        return preflight;
    };

    let outcome =
        execute_managed_runtime_ready_marker_repair(GuardianManagedRuntimeRepairRequest {
            operation_id: repair_boundary.guardian_decision.operation_id.clone(),
            mode: repair_boundary.guardian_decision.mode,
            plan: &repair_plan,
            runtime_root,
            journals: state.journals().as_ref(),
            failure_memory: state.failure_memory().as_ref(),
            observed_at: timestamp_utc().as_str(),
            suppression_until_on_failure: None,
        });

    if outcome.status == GuardianRepairStatus::Failed {
        state.telemetry().emit(TelemetryEvent::error_captured(
            TelemetryErrorKind::GuardianRepairFailed,
            TelemetryErrorArea::Guardian,
            TelemetryErrorLevel::Error,
            outcome.summary.as_str(),
        ));
    }
    let repair_user_outcome = runtime_repair_user_outcome(&outcome);
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
            mark_guardian_runtime_repair_success(
                &mut repaired.guardian_summary,
                &repair_user_outcome,
            );
            repaired
        }
        GuardianRepairStatus::Blocked
        | GuardianRepairStatus::Failed
        | GuardianRepairStatus::Suppressed => {
            block_guardian_for_runtime_repair_outcome(
                &mut preflight.guardian_summary,
                &repair_user_outcome,
            );
            block_preflight_outcome_for_runtime_repair(
                &mut preflight.guardian_outcome,
                &repair_user_outcome,
            );
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
    if !runtime_root.exists() {
        return None;
    }
    let java_executable = managed_runtime_java_executable(&runtime_root);
    if runtime_root.join(".croopor-ready").is_file()
        && managed_runtime_contents_verified_without_probe(&runtime_root)
    {
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

fn mark_guardian_runtime_repair_success(
    summary: &mut GuardianSummary,
    outcome: &GuardianUserOutcome,
) {
    let previous_details = summary.details.clone();
    let previous_guidance = summary.guidance.clone();
    summary.decision = GuardianDecision::Intervened;
    summary.message = Some(outcome.summary.clone());
    summary.details.clear();
    for detail in &outcome.details {
        push_unique_summary_detail(&mut summary.details, detail);
    }
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
    outcome: &GuardianUserOutcome,
) {
    let mut guidance = summary.guidance.clone();
    for detail in &outcome.guidance {
        push_unique_guidance(&mut guidance, detail);
    }
    let reason = outcome
        .details
        .first()
        .map(String::as_str)
        .unwrap_or(outcome.summary.as_str());
    summary.block_with_reason_and_guidance(reason, guidance);
}

fn block_preflight_outcome_for_runtime_repair(
    preflight: &mut GuardianPreflightOutcome,
    outcome: &GuardianUserOutcome,
) {
    preflight.guardian_decision.kind = ApiGuardianDecisionKind::Block;
    preflight.safety.decision = ApiGuardianDecisionKind::Block;
    preflight.safety.summary = outcome.summary.clone();
    preflight.safety.detail = outcome.details.first().cloned();
    preflight.user_outcome.decision = ApiGuardianDecisionKind::Block;
    preflight.user_outcome.summary = outcome.summary.clone();
    preflight.user_outcome.details = outcome.details.clone();
    preflight.user_outcome.guidance = outcome.guidance.clone();
}

fn push_unique_summary_detail(details: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() && !details.iter().any(|detail| detail == value) {
        details.push(value.to_string());
    }
}

fn push_unique_guidance(guidance: &mut Vec<String>, value: &str) {
    if !guidance.iter().any(|existing| existing == value) {
        guidance.push(value.to_string());
    }
}
