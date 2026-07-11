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
use crate::state::contracts::OperationPhase;
use crate::state::{AppState, OperationJournalStoreError};
use axial_config::{AppPaths, Instance};
use axial_launcher::{GuardianDecision, GuardianMode, GuardianSummary};
use axial_minecraft::{
    managed_runtime_contents_verified_without_probe, preferred_runtime_component, resolve_version,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const RUNTIME_REPAIR_RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);

struct RuntimeRepairRequestGuard {
    abandoned: Arc<AtomicBool>,
    armed: bool,
}

impl RuntimeRepairRequestGuard {
    fn new(abandoned: Arc<AtomicBool>) -> Self {
        Self {
            abandoned,
            armed: true,
        }
    }

    fn finish(mut self) {
        self.armed = false;
    }

    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
    }
}

impl Drop for RuntimeRepairRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.abandon();
        }
    }
}

pub(super) async fn maybe_repair_managed_runtime_before_launch_owned(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    mut preflight: LaunchPreflightFacts,
    instance: &Instance,
    library_dir: &Path,
    game_dir: &Path,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
) -> Result<LaunchPreflightFacts, OperationJournalStoreError> {
    if preflight.guardian.mode != GuardianMode::Managed
        || !readiness_has_managed_runtime_missing(&preflight.readiness)
    {
        return Ok(preflight);
    }

    let Some(candidate) = managed_runtime_ready_marker_repair_candidate(
        state.config().paths(),
        library_dir,
        instance,
    ) else {
        return Ok(preflight);
    };
    let Ok(runtime_root) = ManagedRuntimeRoot::from_app_paths(
        state.config().paths(),
        &candidate.runtime_root,
        &candidate.java_executable,
    ) else {
        return Ok(preflight);
    };

    let verification = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(
        runtime_root.target().clone(),
        &candidate.runtime_root,
        &candidate.java_executable,
    ));
    let Err(verification_error) = verification else {
        return Ok(preflight);
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
        return Ok(preflight);
    };

    let state_task = state.clone();
    let runtime_root_path = candidate.runtime_root.clone();
    let java_executable = candidate.java_executable.clone();
    let operation_id = repair_boundary.guardian_decision.operation_id.clone();
    let mode = repair_boundary.guardian_decision.mode;
    let abandoned = Arc::new(AtomicBool::new(false));
    let request_guard = RuntimeRepairRequestGuard::new(abandoned.clone());
    let terminal_failure = Arc::new(tokio::sync::Notify::new());
    let terminal_failure_task = terminal_failure.clone();
    let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.spawn_child(async move {
        let result = match ManagedRuntimeRoot::from_app_paths(
            state_task.config().paths(),
            &runtime_root_path,
            &java_executable,
        ) {
            Ok(runtime_root) => {
                let observed_at = timestamp_utc();
                execute_managed_runtime_ready_marker_repair(GuardianManagedRuntimeRepairRequest {
                    operation_id,
                    mode,
                    plan: &repair_plan,
                    runtime_root,
                    journals: state_task.journals().as_ref(),
                    failure_memory: state_task.failure_memory().as_ref(),
                    observed_at: observed_at.as_str(),
                    suppression_until_on_failure: None,
                    abandoned: Some(abandoned.as_ref()),
                    ready_for_effect: Some(ready_tx),
                    terminal_failure: Some(terminal_failure_task.as_ref()),
                })
                .await
            }
            Err(_) => Err(OperationJournalStoreError::Persistence(
                std::io::Error::other("managed-runtime repair ownership changed before execution"),
            )),
        };
        let _ = result_tx.send(result);
    });
    let mut result_rx = result_rx;
    let outcome = tokio::select! {
        result = &mut result_rx => {
            request_guard.finish();
            result.map_err(|_| {
                OperationJournalStoreError::Persistence(std::io::Error::other(
                    "managed-runtime repair owner stopped before responding",
                ))
            })??
        }
        ready = &mut ready_rx => {
            if ready.is_err() {
                request_guard.finish();
                result_rx.await.map_err(|_| {
                    OperationJournalStoreError::Persistence(std::io::Error::other(
                        "managed-runtime repair owner stopped before effect ownership",
                    ))
                })??
            } else {
                request_guard.finish();
                tokio::select! {
                    result = &mut result_rx => result.map_err(|_| {
                        OperationJournalStoreError::Persistence(std::io::Error::other(
                            "managed-runtime repair owner stopped before responding",
                        ))
                    })??,
                    () = terminal_failure.notified() => {
                        return Err(OperationJournalStoreError::Persistence(std::io::Error::other(
                            "managed-runtime terminal journal reconciliation is still pending",
                        )));
                    }
                }
            }
        }
        () = tokio::time::sleep(RUNTIME_REPAIR_RESPONSE_TIMEOUT) => {
            request_guard.abandon();
            return Err(OperationJournalStoreError::Persistence(
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "managed-runtime repair journal reconciliation is still pending",
                ),
            ));
        }
    };

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
            Ok(repaired)
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
            Ok(preflight)
        }
        GuardianRepairStatus::NotNeeded => Ok(preflight),
    }
}

#[cfg(test)]
pub(super) async fn maybe_repair_managed_runtime_before_launch(
    state: &AppState,
    preflight: LaunchPreflightFacts,
    instance: &Instance,
    library_dir: &Path,
    game_dir: &Path,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
) -> Result<LaunchPreflightFacts, OperationJournalStoreError> {
    let producer = state.try_claim_producer().map_err(|_| {
        OperationJournalStoreError::Persistence(std::io::Error::other(
            "application shutdown is in progress",
        ))
    })?;
    maybe_repair_managed_runtime_before_launch_owned(
        state,
        &producer,
        preflight,
        instance,
        library_dir,
        game_dir,
        requested_max_memory_mb,
        requested_min_memory_mb,
    )
    .await
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
    if runtime_root.join(".axial-ready").is_file()
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
    if cfg!(target_os = "macos") {
        return runtime_root
            .join("jre.bundle")
            .join("Contents")
            .join("Home")
            .join("bin")
            .join("java");
    }

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
