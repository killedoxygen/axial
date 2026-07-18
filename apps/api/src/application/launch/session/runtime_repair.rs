use super::readiness::readiness_has_managed_runtime_missing;
use super::{LaunchPreflightBuild, LaunchPreflightFacts, build_launch_preflight_facts};
use crate::application::guardian_conversion::api_guardian_mode;
use crate::execution::runtime::{
    ManagedRuntimeRoot, ManagedRuntimeVerificationRequest, verify_managed_runtime,
};
use crate::guardian::{
    DiagnosisId, GuardianComponentRebuildOutcome, GuardianComponentRebuildStatus,
    GuardianPreflightOutcomeRequest, GuardianRepairStatus, GuardianRuntimeRepairCopy,
    authorize_managed_runtime_ready_marker_repair, execute_managed_runtime_component_rebuild,
    execute_managed_runtime_ready_marker_repair, guardian_fact_from_execution,
    guardian_preflight_outcome,
};
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
};
use crate::state::contracts::{OperationId, OperationPhase};
use crate::state::{
    AppState, InstanceLifecycleLease, IntegrityForegroundLease, OperationJournalStoreError,
    ReconciliationEvidenceRejection, RegisteredComponentRebuildAdmission,
};
use axial_config::Instance;
use axial_launcher::GuardianMode;
use axial_minecraft::runtime::{ManagedRuntimeRebuildError, RuntimeEnsureEvent};
use axial_minecraft::{ManagedRuntimeCache, preferred_runtime_component, resolve_version};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const RUNTIME_REPAIR_RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);
const RUNTIME_COMPONENT_REBUILD_SUPPRESSION_MINUTES: i64 = 15;

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

#[derive(Clone, Copy)]
pub(super) struct ManagedRuntimeRepairLaunch<'a> {
    pub(super) instance_lifecycle: &'a InstanceLifecycleLease,
    pub(super) instance: &'a Instance,
    pub(super) library_dir: &'a Path,
    pub(super) game_dir: &'a Path,
    pub(super) requested_max_memory_mb: Option<i32>,
    pub(super) requested_min_memory_mb: Option<i32>,
}

pub(super) async fn maybe_repair_managed_runtime_before_launch_owned(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    foreground: &IntegrityForegroundLease,
    preflight: LaunchPreflightFacts,
    launch: ManagedRuntimeRepairLaunch<'_>,
) -> Result<LaunchPreflightFacts, OperationJournalStoreError> {
    maybe_repair_managed_runtime_before_launch_with_source(
        state,
        producer,
        foreground,
        preflight,
        launch,
        RuntimeComponentRebuildSource::Production,
    )
    .await
}

#[cfg(test)]
pub(super) async fn maybe_repair_managed_runtime_before_launch_with_fixture(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    foreground: &IntegrityForegroundLease,
    preflight: LaunchPreflightFacts,
    launch: ManagedRuntimeRepairLaunch<'_>,
) -> Result<LaunchPreflightFacts, OperationJournalStoreError> {
    maybe_repair_managed_runtime_before_launch_with_source(
        state,
        producer,
        foreground,
        preflight,
        launch,
        RuntimeComponentRebuildSource::Fixture,
    )
    .await
}

async fn maybe_repair_managed_runtime_before_launch_with_source(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    foreground: &IntegrityForegroundLease,
    preflight: LaunchPreflightFacts,
    launch: ManagedRuntimeRepairLaunch<'_>,
    rebuild_source: RuntimeComponentRebuildSource,
) -> Result<LaunchPreflightFacts, OperationJournalStoreError> {
    if preflight.guardian.mode != GuardianMode::Managed
        || !readiness_has_managed_runtime_missing(&preflight.readiness)
    {
        return Ok(preflight);
    }

    let Some(candidate) = managed_runtime_ready_marker_repair_candidate(
        state.managed_runtime_cache(),
        launch.library_dir,
        launch.instance,
    ) else {
        return Ok(preflight);
    };
    let Ok(runtime_root) = ManagedRuntimeRoot::from_managed_root(
        state.managed_runtime_cache(),
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
    let repair_outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
        api_guardian_mode(preflight.guardian.mode),
        &guardian_facts,
    ));
    match state.active_recorded_runtime_artifact_failure(launch.instance_lifecycle) {
        Ok(evidence) => {
            let diagnosis_id = Some(evidence.diagnosis_id());
            let admission = state
                .admit_runtime_component_rebuild(
                    evidence,
                    new_runtime_component_rebuild_operation_id(),
                    chrono::Duration::minutes(RUNTIME_COMPONENT_REBUILD_SUPPRESSION_MINUTES),
                )
                .await;
            let repair_foreground = foreground.retained();
            let (effective_status, repair_foreground) = match admission {
                Ok(admission) => {
                    let (component_outcome, repair_foreground) =
                        execute_owned_runtime_component_rebuild(
                            producer,
                            admission,
                            repair_foreground,
                            rebuild_source,
                        )
                        .await?;
                    (
                        component_rebuild_repair_status(component_outcome.status),
                        repair_foreground,
                    )
                }
                Err(_) => (GuardianRepairStatus::Blocked, repair_foreground),
            };
            return finish_managed_runtime_repair(
                state,
                producer,
                ManagedRuntimeRepairCompletion {
                    foreground,
                    preflight,
                    launch,
                    diagnosis_id,
                    effective_status,
                    repair_foreground,
                },
            )
            .await;
        }
        Err(ReconciliationEvidenceRejection::MemoryMissing) => {}
        Err(_) => {
            let diagnosis_id = repair_outcome
                .guardian_decision
                .diagnoses()
                .first()
                .copied();
            return finish_managed_runtime_repair(
                state,
                producer,
                ManagedRuntimeRepairCompletion {
                    foreground,
                    preflight,
                    launch,
                    diagnosis_id,
                    effective_status: GuardianRepairStatus::Blocked,
                    repair_foreground: foreground.retained(),
                },
            )
            .await;
        }
    }

    let Ok(repair_authorization) =
        authorize_managed_runtime_ready_marker_repair(&repair_outcome.guardian_decision)
    else {
        return Ok(preflight);
    };

    let Ok(reconciliation_authority) =
        state.registered_reconciliation_authority(launch.instance_lifecycle)
    else {
        return Ok(preflight);
    };
    let state_task = state.clone();
    let runtime_root_path = candidate.runtime_root.clone();
    let java_executable = candidate.java_executable.clone();
    let operation_id = repair_outcome.guardian_decision.operation_id().cloned();
    let abandoned = Arc::new(AtomicBool::new(false));
    let request_guard = RuntimeRepairRequestGuard::new(abandoned.clone());
    let terminal_failure = Arc::new(tokio::sync::Notify::new());
    let terminal_failure_task = terminal_failure.clone();
    let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let repair_foreground = foreground.retained();
    producer.spawn_child(async move {
        let result = match ManagedRuntimeRoot::from_managed_root(
            state_task.managed_runtime_cache(),
            &runtime_root_path,
            &java_executable,
        ) {
            Ok(runtime_root) => {
                execute_managed_runtime_ready_marker_repair(
                    repair_authorization,
                    operation_id,
                    reconciliation_authority,
                    runtime_root,
                    Some(abandoned.as_ref()),
                    Some(ready_tx),
                    Some(terminal_failure_task.as_ref()),
                )
                .await
            }
            Err(_) => Err(OperationJournalStoreError::Persistence(
                std::io::Error::other("managed-runtime repair ownership changed before execution"),
            )),
        };
        let _ = result_tx.send((result, repair_foreground));
    });
    let mut result_rx = result_rx;
    let response = tokio::select! {
        result = &mut result_rx => {
            request_guard.finish();
            result.map_err(|_| {
                OperationJournalStoreError::Persistence(std::io::Error::other(
                    "managed-runtime repair owner stopped before responding",
                ))
            })?
        }
        ready = &mut ready_rx => {
            if ready.is_err() {
                request_guard.finish();
                result_rx.await.map_err(|_| {
                    OperationJournalStoreError::Persistence(std::io::Error::other(
                        "managed-runtime repair owner stopped before effect ownership",
                    ))
                })?
            } else {
                request_guard.finish();
                tokio::select! {
                    result = &mut result_rx => result.map_err(|_| {
                        OperationJournalStoreError::Persistence(std::io::Error::other(
                            "managed-runtime repair owner stopped before responding",
                        ))
                    })?,
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
    let (outcome, repair_foreground) = response;
    let outcome = outcome?;
    let diagnosis_id = outcome.diagnosis_id;
    let component_evidence = match outcome.status {
        GuardianRepairStatus::Failed => state
            .recorded_runtime_artifact_repair_failure(
                launch.instance_lifecycle,
                &outcome.operation_id,
            )
            .ok(),
        GuardianRepairStatus::Blocked => state
            .active_recorded_runtime_artifact_failure(launch.instance_lifecycle)
            .ok(),
        GuardianRepairStatus::Repaired => None,
    };
    let (effective_status, repair_foreground) = match component_evidence {
        Some(evidence) => {
            let admission = state
                .admit_runtime_component_rebuild(
                    evidence,
                    new_runtime_component_rebuild_operation_id(),
                    chrono::Duration::minutes(RUNTIME_COMPONENT_REBUILD_SUPPRESSION_MINUTES),
                )
                .await;
            match admission {
                Ok(admission) => {
                    let (component_outcome, repair_foreground) =
                        execute_owned_runtime_component_rebuild(
                            producer,
                            admission,
                            repair_foreground,
                            rebuild_source,
                        )
                        .await?;
                    (
                        component_rebuild_repair_status(component_outcome.status),
                        repair_foreground,
                    )
                }
                Err(_) => (GuardianRepairStatus::Blocked, repair_foreground),
            }
        }
        None => (outcome.status, repair_foreground),
    };

    finish_managed_runtime_repair(
        state,
        producer,
        ManagedRuntimeRepairCompletion {
            foreground,
            preflight,
            launch,
            diagnosis_id,
            effective_status,
            repair_foreground,
        },
    )
    .await
}

struct ManagedRuntimeRepairCompletion<'a, 'b> {
    foreground: &'a IntegrityForegroundLease,
    preflight: LaunchPreflightFacts,
    launch: ManagedRuntimeRepairLaunch<'b>,
    diagnosis_id: Option<DiagnosisId>,
    effective_status: GuardianRepairStatus,
    repair_foreground: IntegrityForegroundLease,
}

async fn finish_managed_runtime_repair(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    completion: ManagedRuntimeRepairCompletion<'_, '_>,
) -> Result<LaunchPreflightFacts, OperationJournalStoreError> {
    let ManagedRuntimeRepairCompletion {
        foreground,
        mut preflight,
        launch,
        diagnosis_id,
        effective_status,
        repair_foreground,
    } = completion;
    if effective_status == GuardianRepairStatus::Failed {
        state.telemetry().emit(TelemetryEvent::error_captured(
            TelemetryErrorKind::GuardianRepairFailed,
            TelemetryErrorArea::Guardian,
            TelemetryErrorLevel::Error,
            "managed_runtime_component_rebuild_failed",
        ));
    }
    let repair_copy = GuardianRuntimeRepairCopy::author(diagnosis_id, effective_status)
        .ok_or_else(|| {
            OperationJournalStoreError::Persistence(std::io::Error::other(
                "managed-runtime repair outcome is missing its supported diagnosis",
            ))
        })?;
    let result = match effective_status {
        GuardianRepairStatus::Repaired => {
            let prior_java_probe_receipt = preflight.java_probe_receipt.take();
            let mut repaired = build_launch_preflight_facts(
                state,
                producer,
                LaunchPreflightBuild {
                    integrity_foreground: foreground,
                    instance_lifecycle: launch.instance_lifecycle,
                    instance: launch.instance,
                    config: &preflight.config,
                    library_dir: launch.library_dir,
                    game_dir: launch.game_dir,
                    requested_max_memory_mb: launch.requested_max_memory_mb,
                    requested_min_memory_mb: launch.requested_min_memory_mb,
                },
                prior_java_probe_receipt,
            )
            .await;
            repaired.guardian_summary = repair_copy.guardian_summary(&repaired.guardian_summary);
            Ok(repaired)
        }
        GuardianRepairStatus::Blocked | GuardianRepairStatus::Failed => {
            preflight.guardian_summary = repair_copy.guardian_summary(&preflight.guardian_summary);
            preflight.guardian_admission = repair_copy
                .blocked_admission(&preflight.guardian_outcome)
                .expect("non-repaired runtime copy authors a blocked admission");
            Ok(preflight)
        }
    };
    drop(repair_foreground);
    result
}

#[derive(Clone, Copy)]
pub(super) enum RuntimeComponentRebuildSource {
    Production,
    #[cfg(test)]
    Fixture,
}

struct ManagedRuntimeRepairCandidate {
    runtime_root: PathBuf,
    java_executable: PathBuf,
}

fn managed_runtime_ready_marker_repair_candidate(
    runtime_cache: &ManagedRuntimeCache,
    library_dir: &Path,
    instance: &Instance,
) -> Option<ManagedRuntimeRepairCandidate> {
    let version = resolve_version(library_dir, &instance.version_id).ok()?;
    let component = preferred_runtime_component(&version.java_version);
    let runtime_root = runtime_cache.component_root(&component)?;
    if !runtime_root.exists() {
        return None;
    }
    let java_executable = managed_runtime_java_executable(&runtime_root);
    if runtime_root.join(".axial-ready").is_file() {
        return None;
    }
    Some(ManagedRuntimeRepairCandidate {
        runtime_root,
        java_executable,
    })
}

async fn execute_owned_runtime_component_rebuild(
    producer: &crate::state::ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
    foreground: IntegrityForegroundLease,
    rebuild_source: RuntimeComponentRebuildSource,
) -> Result<(GuardianComponentRebuildOutcome, IntegrityForegroundLease), OperationJournalStoreError>
{
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let rebuild_owner = producer.claim_child();
    producer.spawn_child(async move {
        let result = execute_managed_runtime_component_rebuild(
            rebuild_owner,
            admission,
            move |effect| async move {
                let (runtime_cache, component) = effect.core_request();
                let mut progress = RuntimeComponentRebuildProgress::default();
                let rebuild = match rebuild_source {
                    RuntimeComponentRebuildSource::Production => {
                        axial_minecraft::runtime::rebuild_managed_runtime_component(
                            &runtime_cache,
                            component,
                            |event| progress.observe(&event),
                        )
                        .await
                    }
                    #[cfg(test)]
                    RuntimeComponentRebuildSource::Fixture => {
                        axial_minecraft::rebuild_managed_runtime_fixture_for_test(
                            &runtime_cache,
                            component,
                        )
                        .await
                    }
                };
                match rebuild {
                    Ok(receipt) => effect.succeeded(receipt, progress.fact_ids()),
                    Err(ManagedRuntimeRebuildError::Preparation(_)) => effect.failed_before_effect(
                        progress.failed_fact_ids("runtime_component_rebuild_preparation_failed"),
                    ),
                    Err(ManagedRuntimeRebuildError::Effect(receipt)) => effect.failed_after_effect(
                        receipt,
                        progress.failed_fact_ids("runtime_component_rebuild_effect_failed"),
                    ),
                }
            },
        )
        .await;
        let _ = result_tx.send((result, foreground));
    });
    let (result, foreground) = result_rx.await.map_err(|_| {
        OperationJournalStoreError::Persistence(std::io::Error::other(
            "runtime component rebuild owner stopped before settlement",
        ))
    })?;
    result.map(|outcome| (outcome, foreground))
}

fn component_rebuild_repair_status(status: GuardianComponentRebuildStatus) -> GuardianRepairStatus {
    match status {
        GuardianComponentRebuildStatus::Rebuilt => GuardianRepairStatus::Repaired,
        GuardianComponentRebuildStatus::Failed => GuardianRepairStatus::Failed,
    }
}

#[derive(Default)]
struct RuntimeComponentRebuildProgress {
    downloading: bool,
    installing: bool,
    ready: bool,
}

impl RuntimeComponentRebuildProgress {
    fn observe(&mut self, event: &RuntimeEnsureEvent) {
        match event {
            RuntimeEnsureEvent::DownloadingManagedRuntime { .. } => self.downloading = true,
            RuntimeEnsureEvent::InstallingManagedRuntimeFiles { .. } => self.installing = true,
            RuntimeEnsureEvent::ManagedRuntimeReady { .. } => self.ready = true,
        }
    }

    fn fact_ids(&self) -> Vec<String> {
        let mut facts = Vec::with_capacity(3);
        if self.downloading {
            facts.push("runtime_component_rebuild_downloading".to_string());
        }
        if self.installing {
            facts.push("runtime_component_rebuild_installing".to_string());
        }
        if self.ready {
            facts.push("runtime_component_rebuild_ready".to_string());
        }
        facts
    }

    fn failed_fact_ids(&self, failure: &'static str) -> Vec<String> {
        let mut facts = self.fact_ids();
        facts.push(failure.to_string());
        facts
    }
}

fn new_runtime_component_rebuild_operation_id() -> OperationId {
    OperationId::new(format!(
        "guardian-runtime-component-rebuild-{}",
        uuid::Uuid::new_v4()
    ))
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
