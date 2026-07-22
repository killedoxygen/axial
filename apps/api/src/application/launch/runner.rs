mod failure;
mod metadata;
mod proof;
mod recovery;
mod spawn;
mod status;

use super::{launch_application_stage_evidence, launch_command_stage_evidence};
use crate::application::guardian_conversion::api_guardian_mode;
use crate::application::registered_artifact_recovery::{
    REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES, RegisteredArtifactComponentRebuildSource,
    RegisteredArtifactRecoveryEntry, execute_registered_artifact_recovery_sequence,
    new_registered_artifact_repair_operation_id,
};
use crate::execution::integrity::sense_integrity_tier1;
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairStatus, GuardianCopyRequest,
    GuardianFact, GuardianLaunchRecoveryPlan, GuardianObservedLaunchFailurePhase,
    GuardianPrepareFailureRequest, GuardianPresetAdjustmentRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, GuardianSummary,
    author_guardian_copy, guardian_fact_from_execution,
    guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
    guardian_startup_failure_outcome, guardian_summary_with_artifact_repair_outcome,
    guardian_summary_with_blocked_outcome, guardian_summary_with_observed_outcome,
    is_guardian_launch_crash_class, record_launch_failure_observation, user_mod_set_drift_fact,
};
use crate::logging::timestamp_utc;
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    TelemetryLaunchOutcome,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{
    AppState, LaunchEvent, LaunchFailureTermination, LaunchFailureTerminationErrorClass,
    OperationJournalStoreError, ProcessSettlementLease, RegisteredArtifactFindings,
    RunningHandoffOutcome, StalledStartupTermination, StartupOutcome,
};
use axial_launcher::{
    LaunchFailureClass, LaunchIntent, LaunchPreparationEvent, LaunchSessionExitReason,
    LaunchSessionOutcome, LaunchSessionOutcomeKind, LaunchState, PreparedLaunchAttempt,
    build_healing_summary, prepare_launch_attempt_with_events,
};
use axial_minecraft::download::repair_virtual_assets_from_index;
use axial_minecraft::paths::assets_dir;
use failure::{LaunchFailure, fail_launch, fail_launch_for_journal};
use metadata::persist_launch_metadata;
use proof::persist_launch_proof_with_context_owned as persist_launch_proof_with_context;
use recovery::{
    RecoveryDirectiveOutcome, RecoveryDirectiveRequest, RecoveryDirectiveStage,
    apply_prepare_recovery_directive, apply_startup_recovery_directive, handle_recovery_directive,
    record_failed_self_healing_if_any, record_prelaunch_preset_adjustment_directive,
    record_successful_self_healing_if_any,
};
use spawn::{launch_spawn_failed_stage_evidence, launch_spawn_stage_evidence};
use status::{
    emit_status, launch_state_for_preparation_event, launch_status_event, serialize_guardian,
};
use tokio::process::Command;

pub(super) use failure::sanitize_live_launch_failure_message;
pub(in crate::application::launch) use proof::persist_launch_proof_owned;

pub(super) async fn persist_launch_proof_for_reservation_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    launched_at: Option<&str>,
    proof_context: &LaunchProofContext,
) {
    persist_launch_proof_with_context(
        state,
        producer,
        session_id,
        launched_at,
        "failed",
        Some(proof_context),
    )
    .await;
}

const STARTUP_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_TERMINAL_REATTACH_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const MAX_RECOVERY_ATTEMPTS: u8 = 3;

pub(crate) struct LaunchSuccess {
    pub(super) session_id: String,
    pub(super) instance_id: String,
    pub(super) launched_at: String,
    pub(super) max_memory_mb: i32,
    pub(super) min_memory_mb: i32,
    pub(super) guardian: Option<GuardianSummary>,
}

#[derive(Clone)]
pub(crate) struct LaunchRequestError {
    pub(crate) message: String,
    pub(crate) healing: Option<axial_launcher::LaunchHealingSummary>,
    pub(crate) guardian: Option<GuardianSummary>,
}

enum LaunchTerminalizationDisposition {
    Complete(Result<LaunchSuccess, LaunchRequestError>),
    Retained(Result<LaunchSuccess, LaunchRequestError>),
    Settled(Result<LaunchSuccess, LaunchRequestError>),
}

enum TerminalObservationHandoff {
    Observe { guardian: GuardianSummary },
    Preserve,
}

struct LaunchSessionRunTask {
    preflight_stage_evidence: Vec<axial_launcher::LaunchStageEvidence>,
    session_id: axial_launcher::SessionId,
    instance: axial_config::Instance,
    intent: axial_launcher::LaunchIntent,
    performance_mode: String,
    guardian: GuardianSummary,
    launched_at: String,
    benchmark: Option<crate::state::launch_reports::LaunchBenchmarkMetadata>,
    resource_budget: Option<crate::state::launch_reports::LaunchProofResourceBudget>,
    java_probe_receipt: Option<axial_minecraft::JavaRuntimeProbeReceipt>,
}

impl LaunchSessionRunTask {
    fn from_prepared(
        task: super::session::LaunchSessionTask,
    ) -> (
        crate::state::IntegrityForegroundLease,
        crate::state::UpdateOperationLease,
        Self,
    ) {
        let super::session::LaunchSessionTask {
            update_admission,
            integrity_foreground,
            preflight_stage_evidence,
            session_id,
            instance,
            intent,
            performance_mode,
            guardian,
            launched_at,
            benchmark,
            resource_budget,
            java_probe_receipt,
        } = task;
        (
            integrity_foreground,
            update_admission,
            Self {
                preflight_stage_evidence,
                session_id,
                instance,
                intent,
                performance_mode,
                guardian,
                launched_at,
                benchmark,
                resource_budget,
                java_probe_receipt,
            },
        )
    }
}

pub(crate) async fn launch_session(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_with_control(state, task, producer, LaunchLoopControl::default()).await
}

#[cfg(all(test, unix))]
pub(crate) async fn launch_session_with_persisted_runtime_manifest_for_test(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_with_control(
        state,
        task,
        producer,
        LaunchLoopControl {
            runtime_prepare_source: Some(LaunchRuntimePrepareSource::PersistedManifest),
            ..LaunchLoopControl::default()
        },
    )
    .await
}

async fn launch_session_with_control(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
    control: LaunchLoopControl,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let session_id = task.session_id.0.clone();
    let instance_id = task.instance.id.clone();
    let guardian_mode = api_guardian_mode(task.intent.guardian.mode);
    let initial_guardian = task.guardian.clone();
    let launched_at = task.launched_at.clone();
    let proof_context = LaunchProofContext::from_intent(&task.intent, &task.performance_mode)
        .with_benchmark(task.benchmark.clone())
        .with_resource_budget(task.resource_budget.clone());
    let sessions = state.sessions().clone();
    let (observer_handoff, observer_generation) = if let Some((generation, events)) =
        sessions.subscribe_terminal_observation(&session_id).await
    {
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        let observer_state = state.clone();
        let observer_session_id = session_id.clone();
        let observer_update_admission = task.update_admission.clone();
        let observer_initial_guardian = initial_guardian.clone();
        let settlement_producer = producer.claim_child();
        producer.spawn_child(async move {
            let _update_admission = observer_update_admission;
            own_terminal_observation(
                observer_state,
                observer_session_id,
                instance_id,
                guardian_mode,
                launched_at,
                proof_context,
                generation,
                observer_initial_guardian,
                settlement_producer,
                events,
                handoff_rx,
            )
            .await;
        });
        (Some(handoff_tx), Some(generation))
    } else {
        (None, None)
    };
    let (integrity_foreground, update_admission, task) = LaunchSessionRunTask::from_prepared(task);
    let mut integrity_foreground = Some(integrity_foreground);
    let mut update_admission = Some(update_admission);
    let result = launch_session_inner_with_control(
        state.clone(),
        task,
        &producer,
        &mut integrity_foreground,
        &control,
    )
    .await;
    let disposition = terminalize_unhandled_launch_error(
        &state,
        &producer,
        &session_id,
        result,
        &mut integrity_foreground,
        &mut update_admission,
    )
    .await;
    drop(integrity_foreground);
    drop(update_admission);
    let (handoff, transfer_hold) = match &disposition {
        LaunchTerminalizationDisposition::Complete(Ok(success)) => (
            TerminalObservationHandoff::Observe {
                guardian: success.guardian.clone().unwrap_or(initial_guardian),
            },
            true,
        ),
        LaunchTerminalizationDisposition::Complete(Err(_)) => {
            (TerminalObservationHandoff::Preserve, false)
        }
        LaunchTerminalizationDisposition::Retained(_)
        | LaunchTerminalizationDisposition::Settled(_) => {
            (TerminalObservationHandoff::Preserve, false)
        }
    };
    let handoff_succeeded =
        observer_handoff.is_some_and(|handoff_tx| handoff_tx.send(handoff).is_ok());
    let observer_owns_hold = transfer_hold && handoff_succeeded;

    match disposition {
        LaunchTerminalizationDisposition::Complete(result) => {
            if !observer_owns_hold {
                if let Some(generation) = observer_generation {
                    sessions
                        .release_terminal_observation_hold(&session_id, generation)
                        .await;
                } else {
                    sessions.release_terminal_retention_hold(&session_id).await;
                }
            }
            result
        }
        LaunchTerminalizationDisposition::Retained(result)
        | LaunchTerminalizationDisposition::Settled(result) => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn own_terminal_observation(
    state: AppState,
    session_id: String,
    instance_id: String,
    guardian_mode: crate::guardian::GuardianMode,
    launched_at: String,
    proof_context: LaunchProofContext,
    generation: u64,
    initial_guardian: GuardianSummary,
    settlement_producer: crate::state::ProducerLease,
    mut events: tokio::sync::broadcast::Receiver<LaunchEvent>,
    handoff: tokio::sync::oneshot::Receiver<TerminalObservationHandoff>,
) {
    let (guardian, handoff_dropped) = match handoff.await {
        Ok(TerminalObservationHandoff::Observe { guardian }) => (guardian, false),
        Ok(TerminalObservationHandoff::Preserve) => return,
        Err(_) => {
            if !state
                .sessions()
                .terminal_observation_is_pending(&session_id, generation)
                .await
            {
                state
                    .sessions()
                    .release_terminal_observation_hold(&session_id, generation)
                    .await;
                return;
            }
            (initial_guardian, true)
        }
    };

    let settlement = loop {
        if let Some(lease) = state
            .sessions()
            .claim_process_settlement(&session_id, generation, None)
            .await
        {
            break Some(lease);
        }

        let record = state.sessions().get(&session_id).await;
        if record
            .as_ref()
            .is_some_and(|record| matches!(record.state, LaunchState::Failed | LaunchState::Exited))
        {
            if let Some(record) = record {
                persist_terminal_proof(
                    &state,
                    &session_id,
                    &launched_at,
                    proof_context.clone(),
                    record,
                )
                .await;
            }
            break None;
        }

        if handoff_dropped
            && !state
                .sessions()
                .terminal_observation_is_pending(&session_id, generation)
                .await
        {
            break None;
        }

        let next_event = if handoff_dropped {
            match tokio::time::timeout(SESSION_TERMINAL_REATTACH_DELAY, events.recv()).await {
                Ok(event) => event,
                Err(_) => continue,
            }
        } else {
            events.recv().await
        };
        match next_event {
            Ok(LaunchEvent::ProcessSettled {
                generation: signal_generation,
                attempt_id,
            }) if signal_generation == generation => {
                if let Some(lease) = state
                    .sessions()
                    .claim_process_settlement(&session_id, generation, Some(attempt_id))
                    .await
                {
                    break Some(lease);
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break None;
            }
        }
    };

    if let Some(lease) = settlement {
        let completion_state = state.clone();
        let completion = settlement_producer.spawn_joinable(async move {
            settle_observed_process_exit(
                &completion_state,
                &session_id,
                &instance_id,
                guardian_mode,
                &launched_at,
                proof_context,
                guardian,
                lease,
            )
            .await;
        });
        let _ = completion.await;
        return;
    }

    state
        .sessions()
        .release_terminal_observation_hold(&session_id, generation)
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn settle_observed_process_exit(
    state: &AppState,
    session_id: &str,
    instance_id: &str,
    guardian_mode: crate::guardian::GuardianMode,
    launched_at: &str,
    proof_context: LaunchProofContext,
    mut guardian: GuardianSummary,
    mut lease: ProcessSettlementLease,
) {
    let mut event = lease.event().clone();
    let failure_class = event
        .failure_class
        .as_deref()
        .and_then(LaunchFailureClass::from_name)
        .filter(|failure_class| is_guardian_launch_crash_class(*failure_class));
    if let Some(failure_class) = failure_class {
        if let Some(user_outcome) =
            author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
                failure_class,
                event.crash_evidence.as_ref(),
                GuardianObservedLaunchFailurePhase::AfterBoot,
            ))
        {
            guardian = guardian_summary_with_observed_outcome(&guardian, &user_outcome);
            event.failure_detail = Some(user_outcome.summary().to_string());
            event.guardian = serialize_guardian(Some(guardian));
        }

        let observed_at = timestamp_utc();
        if let Err(error) = record_launch_failure_observation(
            state.failure_memory(),
            instance_id,
            guardian_mode,
            failure_class,
            &observed_at,
        ) {
            tracing::warn!(
                error_kind = error.class(),
                failure_class = failure_class.as_str(),
                "failed to record observed launch failure"
            );
        }
    }

    let proof_record = lease.preview(event.clone());
    persist_terminal_proof(state, session_id, launched_at, proof_context, proof_record).await;
    let _ = lease.finalize(event).await;
    lease.release().await;
}

async fn persist_terminal_proof(
    state: &AppState,
    session_id: &str,
    launched_at: &str,
    proof_context: LaunchProofContext,
    record: crate::state::LaunchSessionRecord,
) {
    let outcome = match record.outcome.as_ref().map(|outcome| outcome.kind) {
        Some(LaunchSessionOutcomeKind::Clean) => "completed",
        Some(LaunchSessionOutcomeKind::Stopped) => return,
        Some(LaunchSessionOutcomeKind::Failed | LaunchSessionOutcomeKind::Unknown) => "failed",
        None if record.state == LaunchState::Failed => "failed",
        None => "exited",
    };
    if let Err(error) = state
        .launch_reports()
        .persist(
            record,
            Some(launched_at.to_string()),
            outcome.to_string(),
            Some(proof_context),
        )
        .await
    {
        tracing::warn!(
            session_id,
            error_kind = ?error.kind(),
            "launch proof persistence failed"
        );
    }
}

async fn terminalize_unhandled_launch_error(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    result: Result<LaunchSuccess, LaunchRequestError>,
    integrity_foreground: &mut Option<crate::state::IntegrityForegroundLease>,
    update_admission: &mut Option<crate::state::UpdateOperationLease>,
) -> LaunchTerminalizationDisposition {
    let error = match result {
        Ok(success) => {
            return LaunchTerminalizationDisposition::Complete(Ok(success));
        }
        Err(error) => error,
    };
    let error = LaunchRequestError {
        message: sanitize_live_launch_failure_message(&error.message),
        healing: error.healing,
        guardian: error.guardian,
    };
    let Some(record) = state.sessions().get(session_id).await else {
        return LaunchTerminalizationDisposition::Complete(Err(error));
    };
    if matches!(record.state, LaunchState::Failed | LaunchState::Exited) {
        return LaunchTerminalizationDisposition::Complete(Err(error));
    }

    match state
        .sessions()
        .terminate_for_launch_failure(session_id)
        .await
    {
        LaunchFailureTermination::Ready(mut lease) => {
            let terminal_error =
                finalize_unhandled_launch_error(state, producer, &mut lease, session_id, error)
                    .await;
            lease.release().await;
            LaunchTerminalizationDisposition::Settled(Err(terminal_error))
        }
        LaunchFailureTermination::Pending(pending) => {
            trace_unconfirmed_launch_failure_termination(pending.error_class());
            let deferred_state = state.clone();
            let deferred_session_id = session_id.to_string();
            let deferred_error = error.clone();
            let deferred_producer = producer.claim_child();
            let retained_foreground = integrity_foreground
                .take()
                .expect("pending preboot terminalization must retain foreground authority");
            let retained_update_admission = update_admission
                .take()
                .expect("pending preboot terminalization must retain update admission");
            producer.spawn_child(async move {
                match pending.wait_for_settlement().await {
                    Ok(mut lease) => {
                        let _ = finalize_unhandled_launch_error(
                            &deferred_state,
                            &deferred_producer,
                            &mut lease,
                            &deferred_session_id,
                            deferred_error,
                        )
                        .await;
                        lease.release().await;
                        drop(retained_foreground);
                        drop(retained_update_admission);
                    }
                    Err(error_class) => {
                        trace_unconfirmed_launch_failure_termination(error_class);
                        retain_launch_authority_until_session_terminal(
                            deferred_state,
                            deferred_session_id,
                            retained_foreground,
                            retained_update_admission,
                        )
                        .await;
                    }
                }
            });
            LaunchTerminalizationDisposition::Retained(Err(error))
        }
        LaunchFailureTermination::Unconfirmed(error_class) => {
            trace_unconfirmed_launch_failure_termination(error_class);
            let retained_state = state.clone();
            let retained_session_id = session_id.to_string();
            let retained_foreground = integrity_foreground
                .take()
                .expect("unconfirmed preboot terminalization must retain foreground authority");
            let retained_update_admission = update_admission
                .take()
                .expect("unconfirmed preboot terminalization must retain update admission");
            producer.spawn_child(async move {
                retain_launch_authority_until_session_terminal(
                    retained_state,
                    retained_session_id,
                    retained_foreground,
                    retained_update_admission,
                )
                .await;
            });
            LaunchTerminalizationDisposition::Retained(Err(error))
        }
    }
}

async fn retain_launch_authority_until_session_terminal(
    state: AppState,
    session_id: String,
    integrity_foreground: crate::state::IntegrityForegroundLease,
    update_admission: crate::state::UpdateOperationLease,
) {
    let _integrity_foreground = integrity_foreground;
    let _update_admission = update_admission;
    let mut changes = state.sessions().subscribe_changes();
    loop {
        let terminal = state
            .sessions()
            .get(&session_id)
            .await
            .is_none_or(|record| matches!(record.state, LaunchState::Failed | LaunchState::Exited));
        if terminal || state.sessions().shutdown_processes_are_settled() {
            return;
        }

        match changes.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tokio::time::sleep(SESSION_TERMINAL_REATTACH_DELAY).await;
                changes = state.sessions().subscribe_changes();
            }
        }
    }
}

async fn finalize_unhandled_launch_error(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    terminalization: &mut crate::state::LaunchFailureTerminalizationLease,
    session_id: &str,
    error: LaunchRequestError,
) -> LaunchRequestError {
    state.telemetry().emit(TelemetryEvent::launch_completed(
        TelemetryLaunchOutcome::Failure,
    ));
    fail_launch_for_journal(
        state,
        producer,
        terminalization,
        session_id,
        &error.message,
        error.healing,
        error.guardian,
    )
    .await
}

fn trace_unconfirmed_launch_failure_termination(error_class: LaunchFailureTerminationErrorClass) {
    tracing::warn!(
        termination_error_class = error_class.as_str(),
        "launch failure termination remains unconfirmed"
    );
}

#[cfg(all(test, unix))]
async fn launch_session_inner(
    state: AppState,
    task: LaunchSessionRunTask,
    producer: &crate::state::ProducerLease,
    integrity_foreground: &mut Option<crate::state::IntegrityForegroundLease>,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_inner_with_control(
        state,
        task,
        producer,
        integrity_foreground,
        &LaunchLoopControl::default(),
    )
    .await
}

async fn launch_session_inner_with_control(
    state: AppState,
    task: LaunchSessionRunTask,
    producer: &crate::state::ProducerLease,
    integrity_foreground: &mut Option<crate::state::IntegrityForegroundLease>,
    control: &LaunchLoopControl,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let LaunchSessionRunTask {
        preflight_stage_evidence,
        session_id,
        instance,
        intent,
        performance_mode,
        mut guardian,
        launched_at,
        benchmark,
        resource_budget,
        java_probe_receipt,
    } = task;
    let session_id = session_id.0;
    let instance_id = instance.id.clone();
    let mut initial_evidence = launch_application_stage_evidence();
    initial_evidence.extend(preflight_stage_evidence);
    state
        .sessions()
        .record_stage_evidence(&session_id, initial_evidence)
        .await;
    if let Some(benchmark_payload) = benchmark
        .as_ref()
        .map(super::launch_benchmark_status_payload)
    {
        state
            .sessions()
            .attach_benchmark(&session_id, benchmark_payload)
            .await;
    }
    let proof_context = LaunchProofContext::from_intent(&intent, &performance_mode)
        .with_benchmark(benchmark)
        .with_resource_budget(resource_budget);
    let mut attempt = axial_launcher::service::AttemptOverrides::default();
    let mut last_recovery_plan: Option<GuardianLaunchRecoveryPlan> = None;
    let mut recovery_attempts = 0_u8;
    let mut registered_recovery_process_retry_used = false;
    let mut launch_completion_pending = false;
    emit_launch_started(
        &state,
        &mut launch_completion_pending,
        Some(intent.loader.clone()),
    );

    loop {
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!("Preparing launch for {}.", instance.name),
            )
            .await;

        let (preparation_event_tx, mut preparation_event_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let preparation_event_sender = preparation_event_tx.clone();
        let preparation_status_state = state.clone();
        let preparation_status_session_id = session_id.clone();
        let preparation_status_guardian = guardian.clone();
        let (preparation_status_done_tx, preparation_status_done_rx) =
            tokio::sync::oneshot::channel();
        producer.spawn_child(async move {
            while let Some(event) = preparation_event_rx.recv().await {
                emit_status(
                    &preparation_status_state,
                    &preparation_status_session_id,
                    launch_state_for_preparation_event(event),
                    None,
                    None,
                    None,
                    Some(preparation_status_guardian.clone()),
                )
                .await;
            }
            let _ = preparation_status_done_tx.send(());
        });
        let prepared_result = if let Some(error) = control.forced_prepare_failure() {
            drop(preparation_event_sender);
            Err(error)
        } else {
            prepare_launch_attempt_for_runner(
                &state,
                &intent,
                &attempt,
                java_probe_receipt.as_ref(),
                control,
                move |event| {
                    let _ = preparation_event_sender.send(event);
                },
            )
            .await
        };
        drop(preparation_event_tx);
        let _ = preparation_status_done_rx.await;

        let prepared = match prepared_result {
            Ok(prepared) => prepared,
            Err(error) => {
                let failure_class = error.failure_class.unwrap_or(LaunchFailureClass::Unknown);
                if let Some(recovery_plan) = last_recovery_plan.take() {
                    record_failed_self_healing_if_any(&state, &session_id, Some(&recovery_plan))
                        .await
                        .map_err(guardian_journal_error)?;
                }
                let prepare_outcome =
                    guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
                        mode: api_guardian_mode(intent.guardian.mode),
                        failure_class,
                        public_error: &error.message,
                        requested_java_present: !intent.requested_java.trim().is_empty(),
                        explicit_java_override_present: intent.guardian.has_java_override(),
                        explicit_jvm_args_present: intent.guardian.has_raw_jvm_args()
                            && !intent.extra_jvm_args.is_empty(),
                        runtime_intervention_applied: attempt.runtime_intervention_applied,
                        raw_jvm_args_intervention_applied: attempt
                            .raw_jvm_args_intervention_applied,
                    });
                if let Some(directive) = prepare_outcome.directive.clone() {
                    let failure_message =
                        match handle_recovery_directive(RecoveryDirectiveRequest {
                            state: &state,
                            session_id: &session_id,
                            instance_id: &instance_id,
                            intent: &intent,
                            directive,
                            stage: RecoveryDirectiveStage::Prepare,
                            mode: api_guardian_mode(intent.guardian.mode),
                            failure_class,
                            recovery_attempts: &mut recovery_attempts,
                            max_recovery_attempts: MAX_RECOVERY_ATTEMPTS,
                            guardian: &mut guardian,
                        })
                        .await
                        .map_err(guardian_journal_error)?
                        {
                            RecoveryDirectiveOutcome::Apply(recovery_plan) => {
                                if control.apply_prepare_recovery_directive(
                                    &mut guardian,
                                    &mut attempt,
                                    &recovery_plan,
                                ) {
                                    last_recovery_plan = Some(recovery_plan);
                                }
                                continue;
                            }
                            RecoveryDirectiveOutcome::Exhausted => {
                                control.record_capped_prepare_failure(&error);
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &prepare_outcome.user_outcome,
                                );
                                prepare_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Rejected => {
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &prepare_outcome.user_outcome,
                                );
                                prepare_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Suppressed(recovery_user_outcome) => {
                                recovery_user_outcome.summary().to_string()
                            }
                        };
                    return Err(finish_launch_failure(
                        &state,
                        producer,
                        &session_id,
                        &mut launch_completion_pending,
                        LaunchFailure {
                            proof_context: Some(&proof_context),
                            class: failure_class,
                            message: &failure_message,
                            healing: error.healing,
                            guardian: Some(guardian.clone()),
                            outcome: None,
                        },
                    )
                    .await);
                }
                guardian =
                    guardian_summary_with_blocked_outcome(&guardian, &prepare_outcome.user_outcome);
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: failure_class,
                        message: &error.message,
                        healing: error.healing,
                        guardian: Some(guardian.clone()),
                        outcome: None,
                    },
                )
                .await);
            }
        };
        if let Some(directive) =
            guardian_prelaunch_preset_adjustment_directive(GuardianPresetAdjustmentRequest {
                mode: api_guardian_mode(intent.guardian.mode),
                requested_preset: &intent.requested_preset,
                effective_preset: &prepared.effective_preset,
                explicit_jvm_preset_present: intent.guardian.has_named_preset(),
            })
        {
            record_prelaunch_preset_adjustment_directive(&mut guardian, &directive);
        }

        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!(
                    "Launch prep finished in {} ms (version {} ms, runtime {} ms, plan {} ms).",
                    prepared.metrics.total_ms,
                    prepared.metrics.version_ms,
                    prepared.metrics.runtime_ms,
                    prepared.metrics.planning_ms,
                ),
            )
            .await;

        emit_status(
            &state,
            &session_id,
            LaunchState::Preparing,
            None,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!(
                    "Using Java {} via {}.",
                    prepared.runtime.effective_info.major, prepared.runtime.effective_source
                ),
            )
            .await;

        let (program, args) = match prepared.plan.command.split_first() {
            Some((program, args)) if !args.is_empty() => {
                state
                    .sessions()
                    .record_stage_evidence(
                        &session_id,
                        launch_command_stage_evidence(true, prepared.plan.command.len()),
                    )
                    .await;
                (program, args)
            }
            _ => {
                record_failed_self_healing_if_any(&state, &session_id, last_recovery_plan.as_ref())
                    .await
                    .map_err(guardian_journal_error)?;
                state
                    .sessions()
                    .record_stage_evidence(
                        &session_id,
                        launch_command_stage_evidence(false, prepared.plan.command.len()),
                    )
                    .await;
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: LaunchFailureClass::Unknown,
                        message: "launch command did not produce a runnable command",
                        healing: prepared.healing.clone(),
                        guardian: Some(guardian.clone()),
                        outcome: None,
                    },
                )
                .await);
            }
        };

        let asset_repair = if prepared.plan.requires_virtual_asset_repair {
            match state.admit_managed_artifact_mutation() {
                Ok(_mutation) => {
                    repair_legacy_virtual_assets_before_launch(&intent.library_dir, &prepared.plan)
                        .await
                }
                Err(error) => Err(axial_minecraft::download::DownloadError::FileOperation(
                    std::io::Error::other(error.to_string()),
                )),
            }
        } else {
            repair_legacy_virtual_assets_before_launch(&intent.library_dir, &prepared.plan).await
        };
        match &asset_repair {
            Ok(outcome) => tracing::debug!(
                session_id,
                full_object_parse_attempts = outcome.full_object_parse_attempts(),
                result = outcome.label(),
                "launch asset repair completed"
            ),
            Err(_) => tracing::debug!(
                session_id,
                full_object_parse_attempts = 1,
                result = "failed",
                "launch asset repair completed"
            ),
        }
        if let Err(error) = asset_repair {
            record_failed_self_healing_if_any(&state, &session_id, last_recovery_plan.as_ref())
                .await
                .map_err(guardian_journal_error)?;
            return Err(finish_launch_failure(
                &state,
                producer,
                &session_id,
                &mut launch_completion_pending,
                LaunchFailure {
                    proof_context: Some(&proof_context),
                    class: LaunchFailureClass::Unknown,
                    message: &error.to_string(),
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                    outcome: None,
                },
            )
            .await);
        }

        let mut command = Command::new(program);
        command.args(args);
        command.current_dir(&prepared.plan.game_dir);

        let record = crate::state::LaunchSessionRecord {
            session_id: axial_launcher::SessionId(session_id.clone()),
            instance_id: instance_id.clone(),
            version_id: intent.version_id.clone(),
            launched_at: Some(launched_at.clone()),
            benchmark: None,
            state: LaunchState::Starting,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: prepared.plan.command.clone(),
            java_path: Some(prepared.runtime.effective_path.clone()),
            natives_dir: prepared
                .plan
                .natives_dir
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            failure: None,
            crash_evidence: None,
            healing: prepared
                .healing
                .as_ref()
                .and_then(|value| serde_json::to_value(value).ok()),
            guardian: serialize_guardian(Some(guardian.clone())),
            outcome: None,
            stages: Vec::new(),
        };

        let launched = match state.sessions().start_process(record, command).await {
            Ok(record) => {
                state
                    .sessions()
                    .record_stage_evidence(
                        &session_id,
                        launch_spawn_stage_evidence(&session_id, &record),
                    )
                    .await;
                record
            }
            Err(error) => {
                record_failed_self_healing_if_any(&state, &session_id, last_recovery_plan.as_ref())
                    .await
                    .map_err(guardian_journal_error)?;
                state
                    .sessions()
                    .record_stage_evidence(&session_id, launch_spawn_failed_stage_evidence())
                    .await;
                emit_pending_launch_failure(&state, &mut launch_completion_pending);
                state.telemetry().emit(TelemetryEvent::error_captured(
                    TelemetryErrorKind::LaunchSpawnFailed,
                    TelemetryErrorArea::Launch,
                    TelemetryErrorLevel::Error,
                    LaunchSessionExitReason::SpawnFailed.summary(),
                ));
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: LaunchFailureClass::Unknown,
                        message: &format!("failed to start launch process: {error}"),
                        healing: prepared.healing.clone(),
                        guardian: Some(guardian.clone()),
                        outcome: Some(LaunchSessionOutcome::from_reason(
                            LaunchSessionExitReason::SpawnFailed,
                        )),
                    },
                )
                .await);
            }
        };
        emit_status(
            &state,
            &session_id,
            LaunchState::Monitoring,
            launched.pid,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;

        let outcome = state
            .sessions()
            .wait_for_startup(&session_id, STARTUP_OBSERVATION_TIMEOUT)
            .await;
        let mut outcome = if outcome == StartupOutcome::Stalled {
            match state
                .sessions()
                .terminate_stalled_startup_attempt(&session_id)
                .await
            {
                Ok(StalledStartupTermination::Settled) => StartupOutcome::Stalled,
                Ok(StalledStartupTermination::StartupCompleted) => StartupOutcome::Stable,
                Err(_) => {
                    return Err(finish_launch_failure(
                        &state,
                        producer,
                        &session_id,
                        &mut launch_completion_pending,
                        LaunchFailure {
                            proof_context: Some(&proof_context),
                            class: LaunchFailureClass::StartupStalled,
                            message: "startup process could not be stopped safely",
                            healing: prepared.healing.clone(),
                            guardian: Some(guardian.clone()),
                            outcome: None,
                        },
                    )
                    .await);
                }
            }
        } else {
            outcome
        };
        control.wait_before_running_handoff().await;
        if matches!(outcome, StartupOutcome::Stable | StartupOutcome::TimedOut) {
            let running = launch_status_event(
                LaunchState::Running,
                launched.pid,
                None,
                prepared.healing.clone(),
                Some(guardian.clone()),
            );
            outcome = match state
                .sessions()
                .publish_running_and_complete_startup_recovery(&launched, running)
                .await
            {
                RunningHandoffOutcome::Published => outcome,
                RunningHandoffOutcome::Settling => StartupOutcome::Settling,
                RunningHandoffOutcome::Stopped => StartupOutcome::Stopped,
                RunningHandoffOutcome::Rejected => StartupOutcome::Exited,
            };
        }

        match outcome {
            StartupOutcome::Stable | StartupOutcome::TimedOut | StartupOutcome::Settling => {
                let settlement_pending = outcome == StartupOutcome::Settling;
                if !settlement_pending {
                    record_successful_self_healing_if_any(
                        &state,
                        &session_id,
                        last_recovery_plan.as_ref(),
                    )
                    .await
                    .map_err(guardian_journal_error)?;
                }
                emit_launch_completed(
                    &state,
                    &mut launch_completion_pending,
                    TelemetryLaunchOutcome::Success,
                );
                if !settlement_pending {
                    persist_launch_proof_with_context(
                        &state,
                        producer,
                        &session_id,
                        Some(launched_at.as_str()),
                        "running",
                        Some(&proof_context),
                    )
                    .await;
                    if let Err(stage) = persist_launch_metadata(
                        &state,
                        integrity_foreground
                            .as_ref()
                            .expect("successful launch must retain foreground through metadata"),
                        &instance.id,
                        &intent.auth.player_name,
                        intent.max_memory_mb,
                        intent.min_memory_mb,
                        &launched_at,
                    )
                    .await
                    {
                        tracing::warn!(?stage, "launch metadata persistence failed");
                    }
                    drop(spawn_successful_user_mod_witness_publication(
                        &state,
                        producer,
                        integrity_foreground.as_ref().expect(
                            "successful launch must retain foreground through witness publication",
                        ),
                        &instance_id,
                    ));
                }
                return Ok(LaunchSuccess {
                    session_id: session_id.clone(),
                    instance_id: instance_id.clone(),
                    launched_at: launched_at.clone(),
                    max_memory_mb: intent.max_memory_mb,
                    min_memory_mb: intent.min_memory_mb,
                    guardian: Some(guardian.clone()),
                });
            }
            StartupOutcome::Stopped => {
                emit_pending_launch_failure(&state, &mut launch_completion_pending);
                return Err(LaunchRequestError {
                    message: "Launch stopped by user.".to_string(),
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                });
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                let terminal_record = if stalled {
                    None
                } else {
                    state.sessions().get(&session_id).await
                };
                let failure_class = if stalled {
                    LaunchFailureClass::StartupStalled
                } else {
                    terminal_record
                        .as_ref()
                        .and_then(|record| record.failure.as_ref().map(|failure| failure.class))
                        .unwrap_or(LaunchFailureClass::Unknown)
                };
                let observation = if stalled {
                    GuardianStartupFailureObservation::Stalled
                } else {
                    GuardianStartupFailureObservation::Exited { failure_class }
                };
                let user_mod_witness_fact = sense_user_mod_witness_drift_after_failure(
                    &state,
                    integrity_foreground
                        .as_ref()
                        .expect("preboot launch must retain foreground authority"),
                    &instance_id,
                    failure_class,
                )
                .await;
                let mut integrity = if registered_recovery_process_retry_used {
                    StartupFailureIntegrity::default()
                } else {
                    sense_startup_failure_integrity(
                        &state,
                        integrity_foreground
                            .as_ref()
                            .expect("preboot launch must retain foreground authority"),
                        &instance_id,
                        &intent.library_dir,
                        failure_class,
                    )
                    .await
                };
                if let Some(fact) = user_mod_witness_fact {
                    integrity.facts.push(fact);
                }
                control.record_startup_integrity(&integrity);
                let guardian_mode = api_guardian_mode(intent.guardian.mode);
                let startup_outcome = {
                    let repair_candidate = (!registered_recovery_process_retry_used)
                        .then(|| integrity.repair_candidate())
                        .flatten();
                    guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                        mode: guardian_mode,
                        observation,
                        crash_evidence: terminal_record
                            .as_ref()
                            .and_then(|record| record.crash_evidence.as_ref()),
                        integrity_facts: &integrity.facts,
                        registered_artifact_repair_candidate: repair_candidate,
                        target_version_id: &intent.target_version_id,
                        runtime_major: prepared.runtime.effective_info.major,
                        requested_java_present: !intent.requested_java.trim().is_empty(),
                        explicit_java_override_present: intent.guardian.has_java_override(),
                        explicit_jvm_args_present: intent.guardian.has_raw_jvm_args()
                            && !intent.extra_jvm_args.is_empty(),
                        explicit_jvm_preset_present: intent.guardian.has_named_preset(),
                        startup_recovery_applied: attempt.startup_recovery_applied,
                        disable_custom_gc: attempt.disable_custom_gc,
                        effective_preset: &prepared.effective_preset,
                    })
                };
                let failure_class = startup_outcome.failure_class;
                if is_guardian_launch_crash_class(failure_class) {
                    let observed_at = timestamp_utc();
                    if let Err(error) = record_launch_failure_observation(
                        state.failure_memory(),
                        &instance_id,
                        guardian_mode,
                        failure_class,
                        &observed_at,
                    ) {
                        tracing::warn!(
                            error_kind = error.class(),
                            failure_class = failure_class.as_str(),
                            "failed to record startup launch failure observation"
                        );
                    }
                }
                if let Some(recovery_plan) = last_recovery_plan.take() {
                    record_failed_self_healing_if_any(&state, &session_id, Some(&recovery_plan))
                        .await
                        .map_err(guardian_journal_error)?;
                }
                state.telemetry().emit(TelemetryEvent::error_captured(
                    TelemetryErrorKind::LaunchStartupFailed,
                    TelemetryErrorArea::Launch,
                    TelemetryErrorLevel::Error,
                    failure_class.as_str(),
                ));

                match registered_artifact_startup_disposition(
                    guardian_mode,
                    startup_outcome.guardian_decision.kind(),
                    registered_recovery_process_retry_used,
                ) {
                    RegisteredArtifactStartupDisposition::TerminalizeRetryFailure => {
                        let healing =
                            startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                        guardian = guardian_summary_with_blocked_outcome(
                            &guardian,
                            &startup_outcome.user_outcome,
                        );
                        return Err(finish_launch_failure(
                            &state,
                            producer,
                            &session_id,
                            &mut launch_completion_pending,
                            LaunchFailure {
                                proof_context: Some(&proof_context),
                                class: failure_class,
                                message: startup_outcome.user_outcome.summary(),
                                healing,
                                guardian: Some(guardian.clone()),
                                outcome: None,
                            },
                        )
                        .await);
                    }
                    RegisteredArtifactStartupDisposition::ExecuteRepair => {
                        let healing =
                            startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                        let Some(findings) = integrity.into_findings() else {
                            warn_registered_artifact_repair_failure(
                                &session_id,
                                RegisteredArtifactRepairFailureReason::EvidenceUnavailable,
                            );
                            return Err(finish_registered_artifact_repair_failure(
                                &state,
                                producer,
                                &session_id,
                                &mut launch_completion_pending,
                                &proof_context,
                                failure_class,
                                healing,
                                guardian,
                                DiagnosisId::LauncherManagedArtifactCorrupt,
                                GuardianArtifactRepairStatus::Blocked,
                            )
                            .await);
                        };
                        let authorization_result =
                            findings.authorize_repair(&startup_outcome.guardian_decision);
                        let authorization = match authorization_result {
                            Ok(authorization) => authorization,
                            Err(_) => {
                                warn_registered_artifact_repair_failure(
                                    &session_id,
                                    RegisteredArtifactRepairFailureReason::AuthorizationRejected,
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Blocked,
                                )
                                .await);
                            }
                        };
                        let operation_id = startup_outcome
                            .guardian_decision
                            .operation_id()
                            .cloned()
                            .unwrap_or_else(new_registered_artifact_repair_operation_id);
                        let Some(recovery_scope) = state
                            .sessions()
                            .recovering_component_mutation_scope(&session_id)
                            .await
                        else {
                            warn_registered_artifact_repair_failure(
                                &session_id,
                                RegisteredArtifactRepairFailureReason::SessionNotCurrent,
                            );
                            return Err(finish_registered_artifact_repair_failure(
                                &state,
                                producer,
                                &session_id,
                                &mut launch_completion_pending,
                                &proof_context,
                                failure_class,
                                healing,
                                guardian,
                                DiagnosisId::LauncherManagedArtifactCorrupt,
                                GuardianArtifactRepairStatus::Blocked,
                            )
                            .await);
                        };
                        let admission = match state
                            .admit_launch_registered_artifact_repair(
                                authorization,
                                operation_id.clone(),
                                chrono::Duration::minutes(
                                    REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES,
                                ),
                                recovery_scope,
                            )
                            .await
                        {
                            Ok(admission) => admission,
                            Err(_) => {
                                warn_registered_artifact_repair_failure(
                                    &session_id,
                                    RegisteredArtifactRepairFailureReason::AdmissionRejected,
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Blocked,
                                )
                                .await);
                            }
                        };
                        let retained_foreground = integrity_foreground.take().expect(
                            "registered artifact recovery must retain foreground authority",
                        );
                        let recovery_state = state.clone();
                        let component_rebuild_source =
                            control.registered_artifact_component_rebuild_source();
                        let recovery_producer = producer.claim_child();
                        let repair_task = producer.claim_child().spawn_joinable(async move {
                            let client = reqwest::Client::new();
                            let outcome = execute_registered_artifact_recovery_sequence(
                                &recovery_state,
                                recovery_producer,
                                RegisteredArtifactRecoveryEntry::Fresh(Box::new(admission)),
                                &client,
                                component_rebuild_source,
                            )
                            .await;
                            (outcome, retained_foreground)
                        });
                        let recovery_outcome = match repair_task.await {
                            Ok((Ok(outcome), retained_foreground)) => {
                                *integrity_foreground = Some(retained_foreground);
                                outcome
                            }
                            Ok((Err(_), retained_foreground)) => {
                                *integrity_foreground = Some(retained_foreground);
                                warn_registered_artifact_repair_failure(
                                    &session_id,
                                    RegisteredArtifactRepairFailureReason::ExecutionFailed,
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Failed,
                                )
                                .await);
                            }
                            Err(_) => {
                                warn_registered_artifact_repair_failure(
                                    &session_id,
                                    RegisteredArtifactRepairFailureReason::OwnerStopped,
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Failed,
                                )
                                .await);
                            }
                        };
                        let repair_user_outcome =
                            author_guardian_copy(GuardianCopyRequest::artifact_repair(
                                recovery_outcome.diagnosis_id,
                                recovery_outcome.effective_status,
                            ))
                            .expect("registered artifact repair copy request is closed");
                        guardian = guardian_summary_with_artifact_repair_outcome(
                            &guardian,
                            &repair_user_outcome,
                        );
                        state
                            .sessions()
                            .emit_log(
                                &session_id,
                                "system",
                                repair_user_outcome.summary().to_string(),
                            )
                            .await;
                        match recovery_outcome.effective_status {
                            GuardianArtifactRepairStatus::Repaired => {
                                if !state
                                    .sessions()
                                    .begin_startup_recovery_retry(&session_id)
                                    .await
                                {
                                    return Err(finish_launch_failure(
                                        &state,
                                        producer,
                                        &session_id,
                                        &mut launch_completion_pending,
                                        LaunchFailure {
                                            proof_context: Some(&proof_context),
                                            class: failure_class,
                                            message: "launch recovery session was no longer current",
                                            healing,
                                            guardian: Some(guardian.clone()),
                                            outcome: None,
                                        },
                                    )
                                    .await);
                                }
                                registered_recovery_process_retry_used = true;
                                continue;
                            }
                            GuardianArtifactRepairStatus::Blocked
                            | GuardianArtifactRepairStatus::Failed => {
                                return Err(finish_launch_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    LaunchFailure {
                                        proof_context: Some(&proof_context),
                                        class: failure_class,
                                        message: repair_user_outcome.summary(),
                                        healing,
                                        guardian: Some(guardian.clone()),
                                        outcome: None,
                                    },
                                )
                                .await);
                            }
                        }
                    }
                    RegisteredArtifactStartupDisposition::ContinueStartupRecovery => {}
                }

                if let Some(directive) = startup_outcome.directive.clone() {
                    let failure_message =
                        match handle_recovery_directive(RecoveryDirectiveRequest {
                            state: &state,
                            session_id: &session_id,
                            instance_id: &instance_id,
                            intent: &intent,
                            directive,
                            stage: RecoveryDirectiveStage::Startup,
                            mode: api_guardian_mode(intent.guardian.mode),
                            failure_class,
                            recovery_attempts: &mut recovery_attempts,
                            max_recovery_attempts: MAX_RECOVERY_ATTEMPTS,
                            guardian: &mut guardian,
                        })
                        .await
                        .map_err(guardian_journal_error)?
                        {
                            RecoveryDirectiveOutcome::Apply(recovery_plan) => {
                                if apply_startup_recovery_directive(
                                    &mut guardian,
                                    &mut attempt,
                                    &recovery_plan,
                                ) && state
                                    .sessions()
                                    .begin_startup_recovery_retry(&session_id)
                                    .await
                                {
                                    last_recovery_plan = Some(recovery_plan);
                                    continue;
                                } else {
                                    guardian = guardian_summary_with_blocked_outcome(
                                        &guardian,
                                        &startup_outcome.user_outcome,
                                    );
                                    startup_outcome.user_outcome.summary().to_string()
                                }
                            }
                            RecoveryDirectiveOutcome::Exhausted => {
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &startup_outcome.user_outcome,
                                );
                                startup_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Rejected => {
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &startup_outcome.user_outcome,
                                );
                                startup_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Suppressed(recovery_user_outcome) => {
                                recovery_user_outcome.summary().to_string()
                            }
                        };
                    let healing =
                        startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                    return Err(finish_launch_failure(
                        &state,
                        producer,
                        &session_id,
                        &mut launch_completion_pending,
                        LaunchFailure {
                            proof_context: Some(&proof_context),
                            class: failure_class,
                            message: &failure_message,
                            healing,
                            guardian: Some(guardian.clone()),
                            outcome: None,
                        },
                    )
                    .await);
                }

                let healing = startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                guardian =
                    guardian_summary_with_blocked_outcome(&guardian, &startup_outcome.user_outcome);
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: failure_class,
                        message: startup_outcome.user_outcome.summary(),
                        healing,
                        guardian: Some(guardian.clone()),
                        outcome: None,
                    },
                )
                .await);
            }
        }
    }
}

async fn prepare_launch_attempt_for_runner<F>(
    state: &AppState,
    intent: &LaunchIntent,
    attempt: &axial_launcher::service::AttemptOverrides,
    java_probe_receipt: Option<&axial_minecraft::JavaRuntimeProbeReceipt>,
    _control: &LaunchLoopControl,
    observer: F,
) -> Result<PreparedLaunchAttempt, axial_launcher::LaunchPreparationError>
where
    F: FnMut(LaunchPreparationEvent),
{
    let runtime_mutation_state = state.clone();
    #[cfg(test)]
    let refuse_runtime_mutation_admission = _control.refuse_runtime_mutation_admission;
    let admit_managed_runtime_mutation = move || {
        #[cfg(test)]
        if refuse_runtime_mutation_admission {
            return Err(axial_minecraft::ManagedRuntimeMutationRefused);
        }
        runtime_mutation_state
            .admit_managed_artifact_mutation()
            .map_err(|_| axial_minecraft::ManagedRuntimeMutationRefused)
    };
    #[cfg(test)]
    if _control.runtime_prepare_source() == LaunchRuntimePrepareSource::PersistedManifest {
        axial_launcher::prepare_launch_attempt_with_persisted_runtime_manifest_for_test(
            state.managed_runtime_cache(),
            intent,
            attempt,
            java_probe_receipt,
            admit_managed_runtime_mutation,
            observer,
        )
        .await
    } else {
        prepare_launch_attempt_with_events(
            state.managed_runtime_cache(),
            intent,
            attempt,
            java_probe_receipt,
            admit_managed_runtime_mutation,
            observer,
        )
        .await
    }
    #[cfg(not(test))]
    prepare_launch_attempt_with_events(
        state.managed_runtime_cache(),
        intent,
        attempt,
        java_probe_receipt,
        admit_managed_runtime_mutation,
        observer,
    )
    .await
}

#[derive(Default)]
struct LaunchLoopControl {
    #[cfg(test)]
    forced_prepare_failure: Option<std::sync::Arc<ForcedPrepareFailure>>,
    #[cfg(test)]
    registered_artifact_component_rebuild_source: Option<RegisteredArtifactComponentRebuildSource>,
    #[cfg(test)]
    observed_startup_integrity:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::guardian::GuardianFactId>>>>,
    #[cfg(test)]
    runtime_prepare_source: Option<LaunchRuntimePrepareSource>,
    #[cfg(test)]
    refuse_runtime_mutation_admission: bool,
    #[cfg(test)]
    before_running_handoff: Option<std::sync::Arc<LaunchLoopBarrier>>,
}

#[cfg(test)]
struct LaunchLoopBarrier {
    reached: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[cfg(test)]
impl LaunchLoopBarrier {
    #[cfg(unix)]
    fn new() -> (
        std::sync::Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, reached_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        (
            std::sync::Arc::new(Self {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Mutex::new(Some(release_rx)),
            }),
            reached_rx,
            release,
        )
    }

    async fn wait(&self) {
        if let Some(reached) = self.reached.lock().expect("barrier reached lock").take() {
            let _ = reached.send(());
        }
        let release = self.release.lock().expect("barrier release lock").take();
        if let Some(release) = release {
            let _ = release.await;
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchRuntimePrepareSource {
    Production,
    PersistedManifest,
}

impl LaunchLoopControl {
    async fn wait_before_running_handoff(&self) {
        #[cfg(test)]
        if let Some(barrier) = self.before_running_handoff.as_ref() {
            barrier.wait().await;
        }
    }

    #[cfg(test)]
    fn runtime_prepare_source(&self) -> LaunchRuntimePrepareSource {
        self.runtime_prepare_source
            .unwrap_or(LaunchRuntimePrepareSource::Production)
    }

    fn registered_artifact_component_rebuild_source(
        &self,
    ) -> RegisteredArtifactComponentRebuildSource {
        #[cfg(test)]
        if let Some(source) = self.registered_artifact_component_rebuild_source {
            return source;
        }
        RegisteredArtifactComponentRebuildSource::Production
    }

    fn record_startup_integrity(&self, _integrity: &StartupFailureIntegrity) {
        #[cfg(test)]
        if let Some(observed) = self.observed_startup_integrity.as_ref() {
            observed
                .lock()
                .expect("startup integrity observation lock")
                .extend(_integrity.facts.iter().map(|fact| fact.id));
        }
    }

    fn forced_prepare_failure(&self) -> Option<axial_launcher::LaunchPreparationError> {
        #[cfg(test)]
        if let Some(failure) = self.forced_prepare_failure.as_ref() {
            return Some(failure.next());
        }
        None
    }

    fn apply_prepare_recovery_directive(
        &self,
        guardian: &mut GuardianSummary,
        attempt: &mut axial_launcher::service::AttemptOverrides,
        recovery_plan: &GuardianLaunchRecoveryPlan,
    ) -> bool {
        #[cfg(test)]
        if self.forced_prepare_failure.is_some() {
            return false;
        }
        apply_prepare_recovery_directive(guardian, attempt, recovery_plan)
    }

    fn record_capped_prepare_failure(&self, _error: &axial_launcher::LaunchPreparationError) {
        #[cfg(test)]
        if let Some(failure) = self.forced_prepare_failure.as_ref() {
            failure.record_capped(&_error.message);
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct ForcedPrepareFailure {
    observed: std::sync::atomic::AtomicU8,
    capped_message: std::sync::Mutex<Option<String>>,
}

#[cfg(test)]
impl ForcedPrepareFailure {
    fn next(&self) -> axial_launcher::LaunchPreparationError {
        let ordinal = self
            .observed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        axial_launcher::LaunchPreparationError {
            message: format!("forced prepare failure {ordinal}"),
            failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
            healing: None,
        }
    }

    fn record_capped(&self, message: &str) {
        *self
            .capped_message
            .lock()
            .expect("forced prepare failure lock poisoned") = Some(message.to_string());
    }
}

fn guardian_journal_error(_error: OperationJournalStoreError) -> LaunchRequestError {
    LaunchRequestError {
        message:
            "Could not record the launch recovery safely. Check app data permissions and try again."
                .to_string(),
        healing: None,
        guardian: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_registered_artifact_repair_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    launch_completion_pending: &mut bool,
    proof_context: &LaunchProofContext,
    failure_class: LaunchFailureClass,
    healing: Option<axial_launcher::LaunchHealingSummary>,
    guardian: GuardianSummary,
    diagnosis_id: DiagnosisId,
    status: GuardianArtifactRepairStatus,
) -> LaunchRequestError {
    let user_outcome =
        author_guardian_copy(GuardianCopyRequest::artifact_repair(diagnosis_id, status))
            .expect("registered artifact repair copy request is closed");
    let guardian = guardian_summary_with_artifact_repair_outcome(&guardian, &user_outcome);
    finish_launch_failure(
        state,
        producer,
        session_id,
        launch_completion_pending,
        LaunchFailure {
            proof_context: Some(proof_context),
            class: failure_class,
            message: user_outcome.summary(),
            healing,
            guardian: Some(guardian),
            outcome: None,
        },
    )
    .await
}

async fn finish_launch_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    launch_completion_pending: &mut bool,
    failure: LaunchFailure<'_>,
) -> LaunchRequestError {
    emit_pending_launch_failure(state, launch_completion_pending);
    fail_launch(state, producer, session_id, failure).await
}

fn emit_launch_started(
    state: &AppState,
    launch_completion_pending: &mut bool,
    loader_key: Option<String>,
) {
    if *launch_completion_pending {
        return;
    }

    state
        .telemetry()
        .emit(TelemetryEvent::launch_started(loader_key));
    *launch_completion_pending = true;
}

fn emit_launch_completed(
    state: &AppState,
    launch_completion_pending: &mut bool,
    outcome: TelemetryLaunchOutcome,
) {
    state
        .telemetry()
        .emit(TelemetryEvent::launch_completed(outcome));
    *launch_completion_pending = false;
}

fn emit_pending_launch_failure(state: &AppState, launch_completion_pending: &mut bool) {
    if !*launch_completion_pending {
        return;
    }

    emit_launch_completed(
        state,
        launch_completion_pending,
        TelemetryLaunchOutcome::Failure,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyVirtualAssetRepairOutcome {
    SkippedModern,
    RepairedLegacy,
}

impl LegacyVirtualAssetRepairOutcome {
    fn full_object_parse_attempts(self) -> usize {
        match self {
            Self::SkippedModern => 0,
            Self::RepairedLegacy => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SkippedModern => "skipped_modern",
            Self::RepairedLegacy => "repaired_legacy",
        }
    }
}

async fn repair_legacy_virtual_assets_before_launch(
    library_dir: &std::path::Path,
    plan: &axial_launcher::VanillaLaunchPlan,
) -> Result<LegacyVirtualAssetRepairOutcome, axial_minecraft::download::DownloadError> {
    if !plan.requires_virtual_asset_repair {
        return Ok(LegacyVirtualAssetRepairOutcome::SkippedModern);
    }
    let asset_index_id = plan.version.asset_index.id.trim();
    let asset_index_path = assets_dir(library_dir)
        .join("indexes")
        .join(format!("{asset_index_id}.json"));
    let repaired = repair_virtual_assets_from_index(library_dir, &asset_index_path).await?;
    if !repaired {
        return Err(axial_minecraft::download::DownloadError::Integrity(
            "asset index legacy flags changed during launch preparation".to_string(),
        ));
    }
    Ok(LegacyVirtualAssetRepairOutcome::RepairedLegacy)
}

fn startup_failure_healing(
    intent: &axial_launcher::LaunchIntent,
    prepared: &PreparedLaunchAttempt,
    attempt: &axial_launcher::service::AttemptOverrides,
    failure_class: LaunchFailureClass,
) -> Option<axial_launcher::LaunchHealingSummary> {
    build_healing_summary(axial_launcher::service::HealingSummaryInput {
        auth_mode: if intent.auth.is_offline() {
            "offline"
        } else {
            "online"
        },
        requested_java_path: &intent.requested_java,
        requested_preset: &intent.requested_preset,
        effective_java_path: Some(prepared.runtime.effective_path.as_str()),
        effective_preset: Some(prepared.effective_preset.as_str()),
        fallback_applied: attempt.fallback_applied.as_deref(),
        retry_count: attempt.retry_count,
        failure_class: Some(failure_class),
    })
}

fn failure_class_needs_tier1_integrity(failure_class: LaunchFailureClass) -> bool {
    matches!(
        failure_class,
        LaunchFailureClass::LauncherManagedArtifactSignature
            | LaunchFailureClass::ClasspathModuleConflict
            | LaunchFailureClass::MissingDependency
    )
}

fn failure_class_needs_user_mod_witness(failure_class: LaunchFailureClass) -> bool {
    matches!(
        failure_class,
        LaunchFailureClass::MissingDependency
            | LaunchFailureClass::ModTransformationFailure
            | LaunchFailureClass::ModAttributedCrash
            | LaunchFailureClass::ClasspathModuleConflict
            | LaunchFailureClass::LoaderBootstrapFailure
    )
}

fn spawn_successful_user_mod_witness_publication(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
) -> tokio::sync::oneshot::Receiver<()> {
    let witness_state = state.clone();
    let witness_foreground = foreground.retained();
    let witness_instance_id = instance_id.to_string();
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    producer.spawn_child(async move {
        if witness_state
            .publish_successful_user_mod_witness(&witness_foreground, &witness_instance_id)
            .await
            .is_err()
        {
            tracing::warn!(
                instance_id = witness_instance_id,
                "successful launch user mod witness publication was unavailable"
            );
        }
        let _ = completed_tx.send(());
    });
    completed_rx
}

async fn sense_user_mod_witness_drift_after_failure(
    state: &AppState,
    foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
    failure_class: LaunchFailureClass,
) -> Option<GuardianFact> {
    if !failure_class_needs_user_mod_witness(failure_class) {
        return None;
    }
    state
        .user_mod_witness_drifted_after_failure(foreground, instance_id)
        .await
        .then(user_mod_set_drift_fact)
}

#[derive(Default)]
struct StartupFailureIntegrity {
    facts: Vec<GuardianFact>,
    findings: Option<RegisteredArtifactFindings>,
}

impl StartupFailureIntegrity {
    fn repair_candidate(&self) -> Option<crate::state::RegisteredArtifactRepairCandidate<'_>> {
        self.findings.as_ref()?.repair_candidate()
    }

    fn into_findings(self) -> Option<RegisteredArtifactFindings> {
        self.findings
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredArtifactStartupDisposition {
    ContinueStartupRecovery,
    ExecuteRepair,
    TerminalizeRetryFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredArtifactRepairFailureReason {
    EvidenceUnavailable,
    AuthorizationRejected,
    SessionNotCurrent,
    AdmissionRejected,
    ExecutionFailed,
    OwnerStopped,
}

impl RegisteredArtifactRepairFailureReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EvidenceUnavailable => "evidence_unavailable",
            Self::AuthorizationRejected => "authorization_rejected",
            Self::SessionNotCurrent => "session_not_current",
            Self::AdmissionRejected => "admission_rejected",
            Self::ExecutionFailed => "execution_failed",
            Self::OwnerStopped => "owner_stopped",
        }
    }
}

fn warn_registered_artifact_repair_failure(
    session_id: &str,
    reason: RegisteredArtifactRepairFailureReason,
) {
    tracing::warn!(
        session_id,
        reason = reason.as_str(),
        "registered artifact repair failed"
    );
}

fn registered_artifact_startup_disposition(
    mode: crate::guardian::GuardianMode,
    decision: GuardianActionKind,
    process_retry_used: bool,
) -> RegisteredArtifactStartupDisposition {
    if process_retry_used {
        return RegisteredArtifactStartupDisposition::TerminalizeRetryFailure;
    }
    if mode == crate::guardian::GuardianMode::Managed && decision == GuardianActionKind::Repair {
        return RegisteredArtifactStartupDisposition::ExecuteRepair;
    }
    RegisteredArtifactStartupDisposition::ContinueStartupRecovery
}

async fn sense_startup_failure_integrity(
    state: &AppState,
    integrity_foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
    library_dir: &std::path::Path,
    failure_class: LaunchFailureClass,
) -> StartupFailureIntegrity {
    if !failure_class_needs_tier1_integrity(failure_class) {
        return StartupFailureIntegrity::default();
    }

    let Ok(lifecycle) = state
        .acquire_integrity_instance_lifecycle(integrity_foreground, instance_id)
        .await
    else {
        return StartupFailureIntegrity::default();
    };
    sense_integrity_tier1(state, integrity_foreground, &lifecycle, library_dir)
        .await
        .map(|admitted| {
            let (report, findings) = admitted.into_parts();
            let facts = report
                .facts
                .iter()
                .map(|fact| {
                    guardian_fact_from_execution(
                        fact,
                        crate::state::contracts::OperationPhase::Launching,
                    )
                })
                .collect();
            StartupFailureIntegrity {
                facts,
                findings: Some(findings),
            }
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::guardian::GuardianDomain;
    use crate::guardian::{
        GuardianActionKind, GuardianMode, GuardianSummaryDecision, guardian_summary_for_test,
        guardian_user_outcome_for_test,
    };
    use crate::observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub};
    #[cfg(unix)]
    use crate::state::contracts::TargetKind;
    use crate::state::contracts::{
        OperationId, OperationPhase, OperationStatus, OwnershipClass, ReconciliationComponent,
        ReconciliationRung, ReconciliationTerminalOutcome, StabilizationSystem, TargetDescriptor,
    };
    #[cfg(unix)]
    use crate::state::failure_memory::{FailureMemorySnapshot, failure_memory_path};
    use crate::state::{
        AppStateInit, InstallStore, LaunchEvent, SessionStore, UpdateApplyAdmissionError,
        reconciliation_attempt_key,
    };
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, Instance, InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_launcher::{
        LaunchAuthContext, LaunchGuardianContext, LaunchIntent, LaunchSessionRecord,
        OverrideOrigin, SessionId,
    };
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use axial_minecraft::runtime::RuntimeId;
    use axial_performance::PerformanceManager;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_TELEMETRY_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    const TEST_TELEMETRY_KEY: &str = "phc_test";
    const MANAGED_LIBRARY_FIXTURE_PATH: &str =
        "libraries/org/axial/fixture/1.0.0/fixture-1.0.0.jar";
    const MANAGED_LIBRARY_FIXTURE_BYTES: &[u8] = b"axial managed Libraries fixture";
    #[cfg(unix)]
    const CRASH_E2E_INSTANCE_ID: &str = "0123456789abcdef";

    #[test]
    fn p00_b09_contract_registered_artifact_startup_disposition_is_managed_only() {
        let cases = [
            (
                GuardianMode::Managed,
                GuardianActionKind::Repair,
                false,
                RegisteredArtifactStartupDisposition::ExecuteRepair,
            ),
            (
                GuardianMode::Custom,
                GuardianActionKind::Repair,
                false,
                RegisteredArtifactStartupDisposition::ContinueStartupRecovery,
            ),
            (
                GuardianMode::Disabled,
                GuardianActionKind::Repair,
                false,
                RegisteredArtifactStartupDisposition::ContinueStartupRecovery,
            ),
            (
                GuardianMode::Managed,
                GuardianActionKind::Block,
                true,
                RegisteredArtifactStartupDisposition::TerminalizeRetryFailure,
            ),
        ];
        for (mode, decision, retry_used, expected) in cases {
            assert_eq!(
                registered_artifact_startup_disposition(mode, decision, retry_used),
                expected
            );
        }
    }
    #[cfg(unix)]
    const CRASH_E2E_FABRIC_VERSION_ID: &str =
        "loader-v2-YXhpYWwtaW5zdGFsbGVkLWxvYWRlcgABAAYxLjIxLjEABzAuMTYuMTA";
    #[cfg(unix)]
    const CRASH_E2E_FABRIC_LIBRARIES: [(&str, &str); 2] = [
        (
            "net.fabricmc:fabric-loader:0.16.10",
            "net/fabricmc/fabric-loader/0.16.10/fabric-loader-0.16.10.jar",
        ),
        (
            "net.fabricmc:intermediary:1.21.1",
            "net/fabricmc/intermediary/1.21.1/intermediary-1.21.1.jar",
        ),
    ];

    #[tokio::test]
    async fn ready_managed_runtime_launch_preparation_keeps_artifact_epoch() {
        let root = unique_test_dir("launch-ready-runtime-epoch");
        let state = test_app_state(&root);
        let intent = write_managed_runtime_launch_fixture(&root);
        let receipt = axial_minecraft::rebuild_managed_runtime_fixture_for_test(
            state.managed_runtime_cache(),
            RuntimeId::from("java-runtime-delta"),
        )
        .await
        .expect("ready managed runtime fixture");
        drop(receipt);
        let epoch_before = state
            .managed_artifact_mutation_epoch()
            .expect("managed artifact epoch");
        let mut events = Vec::new();

        let prepared = prepare_launch_attempt_for_runner(
            &state,
            &intent,
            &axial_launcher::service::AttemptOverrides::default(),
            None,
            &LaunchLoopControl {
                runtime_prepare_source: Some(LaunchRuntimePrepareSource::PersistedManifest),
                ..LaunchLoopControl::default()
            },
            |event| events.push(event),
        )
        .await
        .expect("ready managed runtime preparation");

        assert_eq!(
            state.managed_artifact_mutation_epoch(),
            Ok(epoch_before),
            "a healthy runtime must not invalidate managed-artifact readers"
        );
        assert_eq!(prepared.runtime.effective_source, "managed");
        assert!(!events.contains(&LaunchPreparationEvent::DownloadingRuntime));
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_managed_runtime_launch_preparation_advances_artifact_epoch_once() {
        let root = unique_test_dir("launch-missing-runtime-epoch");
        let state = test_app_state(&root);
        let intent = write_managed_runtime_launch_fixture(&root);
        let java_bytes = b"managed runtime launch fixture";
        let (java_url, server) = serve_launch_runtime_once(java_bytes).await;
        let runtime_root = axial_minecraft::persist_managed_runtime_source_fixture_for_test(
            state.managed_runtime_cache(),
            RuntimeId::from("java-runtime-delta"),
            java_url,
            java_bytes,
        )
        .expect("persisted runtime source");
        let epoch_before = state
            .managed_artifact_mutation_epoch()
            .expect("managed artifact epoch");
        let mut events = Vec::new();

        let prepared = prepare_launch_attempt_for_runner(
            &state,
            &intent,
            &axial_launcher::service::AttemptOverrides::default(),
            None,
            &LaunchLoopControl {
                runtime_prepare_source: Some(LaunchRuntimePrepareSource::PersistedManifest),
                ..LaunchLoopControl::default()
            },
            |event| events.push(event),
        )
        .await
        .expect("missing managed runtime preparation");
        server.await.expect("runtime fixture server");

        let epoch_after = state
            .managed_artifact_mutation_epoch()
            .expect("managed artifact epoch after install");
        assert_eq!(epoch_after.value(), epoch_before.value() + 1);
        assert_eq!(prepared.runtime.effective_source, "managed");
        assert!(events.contains(&LaunchPreparationEvent::DownloadingRuntime));
        assert!(managed_runtime_java_path_for_runner_test(&runtime_root).is_file());
        assert!(runtime_root.join(".axial-ready").is_file());
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refused_runtime_mutation_is_terminal_and_has_no_artifact_effects() {
        let root = unique_test_dir("launch-runtime-admission-refusal");
        let state = test_app_state(&root);
        let mut intent = write_managed_runtime_launch_fixture(&root);
        intent.requested_java = "/explicit/java".to_string();
        intent.guardian.java_override_origin = Some(OverrideOrigin::Instance);
        let runtime_root = axial_minecraft::persist_managed_runtime_source_fixture_for_test(
            state.managed_runtime_cache(),
            RuntimeId::from("java-runtime-delta"),
            "http://127.0.0.1:9/java".to_string(),
            b"must not download",
        )
        .expect("persisted runtime source");
        let epoch_before = state
            .managed_artifact_mutation_epoch()
            .expect("managed artifact epoch");
        let attempt = axial_launcher::service::AttemptOverrides {
            force_managed_runtime: true,
            ..Default::default()
        };

        let error = prepare_launch_attempt_for_runner(
            &state,
            &intent,
            &attempt,
            None,
            &LaunchLoopControl {
                runtime_prepare_source: Some(LaunchRuntimePrepareSource::PersistedManifest),
                refuse_runtime_mutation_admission: true,
                ..LaunchLoopControl::default()
            },
            |_| {},
        )
        .await
        .expect_err("managed runtime mutation refusal");

        assert_eq!(
            error.message,
            "resolve java: managed runtime mutation was refused before effects"
        );
        assert_eq!(error.failure_class, Some(LaunchFailureClass::Unknown));
        assert_eq!(
            state.managed_artifact_mutation_epoch(),
            Ok(epoch_before),
            "a refused callback must not advance the epoch"
        );
        assert!(
            !axial_minecraft::runtime_component_executable_present_without_probe(
                state.managed_runtime_cache(),
                "java-runtime-delta",
            )
        );
        assert!(!runtime_root.join(".axial-ready").exists());

        let outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: api_guardian_mode(intent.guardian.mode),
            failure_class: error.failure_class.expect("bounded failure class"),
            public_error: &error.message,
            requested_java_present: true,
            explicit_java_override_present: true,
            explicit_jvm_args_present: false,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });
        assert_eq!(outcome.guardian_decision.kind(), GuardianActionKind::Block);
        assert!(outcome.directive.is_none());
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tier1_integrity_has_an_exhaustive_closed_failure_class_trigger() {
        for &failure_class in LaunchFailureClass::ALL {
            assert_eq!(
                failure_class_needs_tier1_integrity(failure_class),
                matches!(
                    failure_class,
                    LaunchFailureClass::LauncherManagedArtifactSignature
                        | LaunchFailureClass::ClasspathModuleConflict
                        | LaunchFailureClass::MissingDependency
                ),
                "unexpected Tier1 admission for {}",
                failure_class.as_str()
            );
        }
    }

    #[test]
    fn user_mod_witness_has_an_exhaustive_closed_failure_class_trigger() {
        for &failure_class in LaunchFailureClass::ALL {
            assert_eq!(
                failure_class_needs_user_mod_witness(failure_class),
                matches!(
                    failure_class,
                    LaunchFailureClass::MissingDependency
                        | LaunchFailureClass::ModTransformationFailure
                        | LaunchFailureClass::ModAttributedCrash
                        | LaunchFailureClass::ClasspathModuleConflict
                        | LaunchFailureClass::LoaderBootstrapFailure
                ),
                "unexpected user mod witness admission for {}",
                failure_class.as_str()
            );
        }
    }

    #[tokio::test]
    async fn production_user_mod_witness_path_is_mode_orthogonal_and_condition_only() {
        fn outcome(
            mode: GuardianMode,
            integrity_facts: &[GuardianFact],
        ) -> crate::guardian::GuardianLaunchFailureOutcome {
            guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                mode,
                observation: GuardianStartupFailureObservation::Exited {
                    failure_class: LaunchFailureClass::MissingDependency,
                },
                crash_evidence: None,
                integrity_facts,
                registered_artifact_repair_candidate: None,
                target_version_id: "1.21.1",
                runtime_major: 21,
                requested_java_present: false,
                explicit_java_override_present: false,
                explicit_jvm_args_present: false,
                explicit_jvm_preset_present: false,
                startup_recovery_applied: false,
                disable_custom_gc: false,
                effective_preset: "performance",
            })
        }

        let root = unique_test_dir("production-user-mod-witness");
        let state = test_app_state(&root);
        let instance = state
            .instances()
            .insert_for_test("User mod witness", "1.21.1")
            .expect("insert witness instance");
        let instance_mode = instance.performance_mode.clone();
        let mods_dir = state.instances().game_dir(&instance.id).join("mods");
        fs::create_dir_all(&mods_dir).expect("create witness mods root");
        fs::write(mods_dir.join("user.jar"), b"baseline").expect("write baseline user mod");
        let producer = state.try_claim_producer().expect("claim witness producer");
        let publication_foreground = state
            .register_integrity_foreground()
            .expect("register publication foreground")
            .wait_for_settlement()
            .await;
        let publication = spawn_successful_user_mod_witness_publication(
            &state,
            &producer,
            &publication_foreground,
            &instance.id,
        );
        drop(publication_foreground);
        tokio::time::timeout(Duration::from_secs(2), publication)
            .await
            .expect("producer-owned publication deadline")
            .expect("producer-owned publication completed");

        fs::write(mods_dir.join("added.jar"), b"added").expect("add drifted user mod");
        let comparison_foreground = state
            .register_integrity_foreground()
            .expect("register comparison foreground")
            .wait_for_settlement()
            .await;
        assert!(
            sense_user_mod_witness_drift_after_failure(
                &state,
                &comparison_foreground,
                &instance.id,
                LaunchFailureClass::OutOfMemory,
            )
            .await
            .is_none(),
            "ineligible preboot failure must omit the witness"
        );
        let drift_fact = sense_user_mod_witness_drift_after_failure(
            &state,
            &comparison_foreground,
            &instance.id,
            LaunchFailureClass::MissingDependency,
        )
        .await
        .expect("eligible preboot failure compares the successful baseline");

        for &mode in GuardianMode::ALL {
            let baseline = outcome(mode, &[]);
            let with_drift = outcome(mode, std::slice::from_ref(&drift_fact));
            assert_eq!(baseline.guardian_decision, with_drift.guardian_decision);
            assert_eq!(baseline.directive, with_drift.directive);
            assert!(with_drift.user_outcome.details().contains(
                &"The active mods changed since the last successful launch.".to_string()
            ));
            assert!(
                with_drift
                    .user_outcome
                    .details()
                    .iter()
                    .all(|line| !line.contains("added.jar"))
            );
        }
        assert_eq!(
            state
                .instances()
                .get(&instance.id)
                .map(|current| current.performance_mode),
            Some(instance_mode),
            "the witness must not consult or rewrite instance performance mode"
        );

        drop(comparison_foreground);
        drop(producer);
        tokio::time::timeout(Duration::from_secs(5), state.shutdown())
            .await
            .expect("witness shutdown deadline")
            .expect("witness shutdown settles");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn library_component_escalation_retains_exact_transaction_proofs() {
        let root = unique_test_dir("libraries-component-rebuild");
        let instance_id = "0000000000000001";
        let (state, active_inventory) =
            test_libraries_recovery_app_state(&root, instance_id, "http://127.0.0.1:0/unreachable");
        let library_root = PathBuf::from(
            state
                .library_dir()
                .expect("State-authored Libraries recovery root"),
        );
        let foreground = state
            .register_integrity_foreground()
            .expect("register Libraries recovery foreground")
            .wait_for_settlement()
            .await;
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let operation_id = OperationId::deterministic_test("registered-library-leaf-failed");
        let report = sense_integrity_tier1(&state, &foreground, &lifecycle, &library_root)
            .await
            .expect("sense missing registered Libraries fixture");
        assert_eq!(report.facts.len(), 1);
        let (_, findings) = report.into_parts();
        let target = findings
            .repair_candidate()
            .map(|candidate| candidate.target())
            .expect("source-backed registered Libraries target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_repair_decision(
                "registered-library-leaf-failed",
                target,
            ))
            .expect("authorize exact registered Libraries repair");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                operation_id.clone(),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("admit registered Libraries repair");
        drop(lifecycle);

        let recovery = execute_registered_artifact_recovery_sequence(
            &state,
            state
                .try_claim_producer()
                .expect("claim Libraries recovery owner"),
            RegisteredArtifactRecoveryEntry::Fresh(Box::new(admission)),
            &reqwest::Client::new(),
            RegisteredArtifactComponentRebuildSource::Fixture,
        )
        .await
        .expect("settle registered Libraries component recovery");

        assert_eq!(
            recovery.diagnosis_id,
            DiagnosisId::LauncherManagedArtifactCorrupt
        );
        assert_eq!(
            recovery.effective_status,
            GuardianArtifactRepairStatus::Repaired
        );
        let leaf_entry = state
            .journals()
            .get(&operation_id)
            .expect("registered Libraries leaf journal");
        let leaf_attempt = leaf_entry
            .reconciliation_attempt()
            .expect("registered Libraries leaf attempt");
        let leaf_terminal = leaf_entry
            .reconciliation_terminal()
            .expect("registered Libraries leaf terminal");
        assert_eq!(leaf_attempt.rung(), ReconciliationRung::RepairArtifact);
        assert_eq!(leaf_attempt.component(), ReconciliationComponent::Libraries);
        assert_eq!(
            leaf_terminal.outcome(),
            ReconciliationTerminalOutcome::Failed
        );
        assert_eq!(
            state
                .failure_memory()
                .get(&reconciliation_attempt_key(leaf_attempt))
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(leaf_terminal.clone())
        );
        let component_entry = state
            .journals()
            .list()
            .into_iter()
            .find(|entry| {
                entry
                    .reconciliation_attempt()
                    .is_some_and(|attempt| attempt.rung() == ReconciliationRung::RebuildComponent)
            })
            .expect("Libraries component rebuild journal");
        assert_eq!(component_entry.status, OperationStatus::Succeeded);
        assert!(
            component_entry
                .planned_steps
                .iter()
                .any(|step| { step.step_id == "rebuild_managed_libraries_component" })
        );
        assert!(
            component_entry
                .planned_steps
                .iter()
                .all(|step| !step.step_id.contains("quarantine"))
        );
        let component_attempt = component_entry
            .reconciliation_attempt()
            .expect("Libraries component attempt");
        let component_terminal = component_entry
            .reconciliation_terminal()
            .expect("Libraries component terminal");
        assert_eq!(
            component_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert_eq!(
            state
                .failure_memory()
                .get(&reconciliation_attempt_key(component_attempt))
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(component_terminal.clone())
        );
        assert_eq!(
            fs::read(library_root.join(MANAGED_LIBRARY_FIXTURE_PATH))
                .expect("rebuilt Libraries fixture"),
            MANAGED_LIBRARY_FIXTURE_BYTES
        );
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let verification = state
            .mint_known_good_verification_lease(&foreground, &lifecycle, &library_root)
            .expect("current Libraries verification lease");
        assert!(std::ptr::eq(
            verification.execution_parts().5,
            Arc::as_ref(&active_inventory),
        ));
        drop(verification);
        let postcheck = sense_integrity_tier1(&state, &foreground, &lifecycle, &library_root)
            .await
            .expect("fresh Libraries Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        drop((postcheck, lifecycle));

        drop(foreground);
        state
            .close_known_good_inventories()
            .await
            .expect("close Libraries recovery known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close Libraries recovery instance registry");
        state
            .journals()
            .close()
            .await
            .expect("close Libraries recovery journal");
        state
            .failure_memory()
            .close()
            .await
            .expect("close Libraries recovery memory");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn p00_b09_contract_deleted_library_uses_r1_and_second_process_reaches_boot() {
        let root = unique_test_dir("deleted-library-launch-continuation");
        let instance_id = "0000000000000001";
        let session_id = "deleted-library-launch-continuation";
        let (repair_source, source_server) =
            serve_registered_library_once(MANAGED_LIBRARY_FIXTURE_BYTES).await;
        let (state, _) = test_libraries_recovery_app_state(&root, instance_id, &repair_source);
        let library_root = PathBuf::from(
            state
                .library_dir()
                .expect("State-authored deleted-Libraries root"),
        );
        let library_path = library_root.join(MANAGED_LIBRARY_FIXTURE_PATH);
        let process_count_path = root.join("deleted-library-process-count");
        let java_path =
            write_deleted_library_launch_fixture(&root, &library_path, &process_count_path);
        let user_owned = write_user_owned_launch_sentinels(&state, instance_id);
        let mut session = test_record(session_id);
        session.instance_id = instance_id.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert deleted-Libraries launch session");
        let producer = state
            .try_claim_producer()
            .expect("claim deleted-Libraries launch producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, instance_id);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(state.instances().game_dir(instance_id));
        let (integrity_foreground, _update_admission, task) =
            LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);

        let launch_result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner(state.clone(), task, &producer, &mut integrity_foreground),
        )
        .await
        .expect("deleted-Libraries launch deadline");
        let launched = match launch_result {
            Ok(launched) => launched,
            Err(error) => {
                let terminal = state.sessions().get(session_id).await;
                panic!(
                    "deleted-Libraries recovery must reach process 2: {}; terminal={terminal:?}",
                    error.message
                );
            }
        };

        assert_eq!(launched.session_id, session_id);
        assert_eq!(
            fs::read_to_string(&process_count_path).expect("launch process count"),
            "2"
        );
        assert_eq!(
            fs::read(&library_path).expect("leaf-repaired Libraries fixture"),
            MANAGED_LIBRARY_FIXTURE_BYTES
        );
        assert_eq!(user_owned.len(), 5);
        assert_user_owned_launch_sentinels(&user_owned);
        let reconciliation = state
            .journals()
            .list()
            .into_iter()
            .filter_map(|journal| {
                let attempt = journal.reconciliation_attempt()?.clone();
                Some((journal, attempt))
            })
            .collect::<Vec<_>>();
        assert_eq!(reconciliation.len(), 1);
        let leaf = reconciliation
            .first()
            .expect("deleted-Libraries R1 terminal");
        assert_eq!(leaf.1.rung(), ReconciliationRung::RepairArtifact);
        assert_eq!(leaf.0.status, OperationStatus::Succeeded);
        assert_eq!(leaf.1.component(), ReconciliationComponent::Libraries);
        let leaf_terminal = leaf
            .0
            .reconciliation_terminal()
            .expect("deleted-Libraries R1 result");
        assert_eq!(
            leaf_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert!(leaf_terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            state
                .failure_memory()
                .get(&reconciliation_attempt_key(&leaf.1))
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(leaf_terminal.clone())
        );

        let foreground = integrity_foreground
            .as_ref()
            .expect("successful launch retains integrity foreground");
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let postcheck = sense_integrity_tier1(&state, foreground, &lifecycle, &library_root)
            .await
            .expect("deleted-Libraries Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        drop((postcheck, lifecycle));
        source_server
            .await
            .expect("deleted-Libraries source server task");

        let _ = state.sessions().kill(session_id).await;
        drop(integrity_foreground);
        state
            .close_known_good_inventories()
            .await
            .expect("close deleted-Libraries known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close deleted-Libraries instance registry");
        state
            .journals()
            .close()
            .await
            .expect("close deleted-Libraries journal");
        state
            .failure_memory()
            .close()
            .await
            .expect("close deleted-Libraries memory");
        drop((producer, state));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn p00_b09_contract_cross_owner_wrong_client_uses_r1_r2_and_second_process_reaches_boot()
    {
        let root = unique_test_dir("wrong-content-client-launch-continuation");
        let instance_id = "0000000000000001";
        let session_id = "wrong-content-client-launch-continuation";
        let (state, client_path, expected_client) =
            test_version_bundle_recovery_app_state(&root, instance_id);
        let wrong_client = fs::read(&client_path).expect("wrong-content VersionBundle client");
        assert_eq!(wrong_client.len(), expected_client.len());
        assert_ne!(wrong_client, expected_client);
        let process_count_path = root.join("wrong-content-client-process-count");
        let java_path = write_version_bundle_launch_fixture(
            &root,
            &client_path,
            &expected_client,
            &process_count_path,
        );
        let user_owned = write_user_owned_launch_sentinels(&state, instance_id);
        let mut session = test_record(session_id);
        session.instance_id = instance_id.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert wrong-content client launch session");
        let mut launch_events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe wrong-content launch events");
        let producer = state
            .try_claim_producer()
            .expect("claim wrong-content client launch producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, instance_id);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(state.instances().game_dir(instance_id));
        let (integrity_foreground, _update_admission, task) =
            LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);
        let observed_startup_integrity = Arc::new(std::sync::Mutex::new(Vec::new()));
        let control = LaunchLoopControl {
            registered_artifact_component_rebuild_source: Some(
                RegisteredArtifactComponentRebuildSource::Fixture,
            ),
            observed_startup_integrity: Some(observed_startup_integrity.clone()),
            ..LaunchLoopControl::default()
        };

        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner_with_control(
                state.clone(),
                task,
                &producer,
                &mut integrity_foreground,
                &control,
            ),
        )
        .await
        .expect("wrong-content client launch deadline")
        .unwrap_or_else(|error| {
            panic!(
                "VersionBundle recovery must reach process 2: {}",
                error.message
            )
        });

        assert_eq!(launched.session_id, session_id);
        assert_eq!(
            fs::read_to_string(&process_count_path).expect("VersionBundle process count"),
            "2"
        );
        assert_eq!(
            fs::read(&client_path).expect("rebuilt VersionBundle client"),
            expected_client
        );
        let running = state
            .sessions()
            .get(session_id)
            .await
            .expect("running VersionBundle launch");
        assert_eq!(running.state, LaunchState::Running);
        assert!(running.boot_completed_at_ms.is_some());
        assert!(
            observed_startup_integrity
                .lock()
                .expect("observed startup integrity")
                .contains(&crate::guardian::GuardianFactId::ArtifactHashMismatch),
            "process-triggered Tier1 must observe the same-size client hash mismatch"
        );
        let mut statuses = Vec::new();
        loop {
            match launch_events.try_recv() {
                Ok(LaunchEvent::Status(status)) => statuses.push(*status),
                Ok(LaunchEvent::Log(_)) => {}
                Ok(LaunchEvent::ProcessSettled { .. }) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                    panic!("wrong-content launch event proof lagged by {count}")
                }
            }
        }
        let states = statuses
            .iter()
            .map(|status| status.state.as_str())
            .collect::<Vec<_>>();
        assert!(states.contains(&"recovering"));
        assert!(states.contains(&"running"));
        assert!(
            !states
                .iter()
                .any(|state| matches!(*state, "failed" | "exited")),
            "recoverable attempt published a logical terminal: {states:?}"
        );
        let running_status = statuses
            .iter()
            .rev()
            .find(|status| status.state == "running")
            .expect("repaired running status");
        assert_eq!(
            running_status
                .guardian
                .as_ref()
                .and_then(|guardian| guardian["decision"].as_str()),
            Some("intervened")
        );
        assert_eq!(running_status.notice, None);
        assert!(
            super::super::reports::public_launch_status(running_status)
                .notice
                .is_some()
        );
        let running_record = state
            .sessions()
            .get(session_id)
            .await
            .expect("running recovery session");
        let stages = &running_record.stages;
        assert!(
            stages
                .iter()
                .any(|stage| stage.stage == "monitoring"
                    && stage.result.as_deref() == Some("failed"))
        );
        assert!(
            stages
                .iter()
                .any(|stage| stage.stage == "recovering" && stage.result.as_deref() == Some("ok"))
        );

        let reconciliation = state
            .journals()
            .list()
            .into_iter()
            .filter_map(|journal| {
                let attempt = journal.reconciliation_attempt()?.clone();
                Some((journal, attempt))
            })
            .collect::<Vec<_>>();
        assert_eq!(reconciliation.len(), 2);
        let leaf = reconciliation
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RepairArtifact)
            .expect("wrong-content VersionBundle R1 terminal");
        let component = reconciliation
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RebuildComponent)
            .expect("wrong-content VersionBundle R2 terminal");
        assert_eq!(leaf.1.component(), ReconciliationComponent::VersionBundle);
        assert_eq!(
            component.1.component(),
            ReconciliationComponent::VersionBundle
        );
        assert_eq!(leaf.0.status, OperationStatus::Failed);
        assert_eq!(component.0.status, OperationStatus::Succeeded);
        assert_eq!(
            leaf.0.failure_point.as_deref(),
            Some(crate::state::REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
        );
        assert!(leaf.0.planned_steps.iter().all(|step| {
            step.step_id != "download_artifact_to_temp"
                && step.step_id != "quarantine_launcher_managed_target"
        }));
        assert!(
            component.0.planned_steps.iter().any(|step| {
                step.step_id == crate::state::VERSION_BUNDLE_COMPONENT_REBUILD_STEP
            })
        );
        let leaf_terminal = leaf
            .0
            .reconciliation_terminal()
            .expect("wrong-content VersionBundle R1 result");
        let component_terminal = component
            .0
            .reconciliation_terminal()
            .expect("wrong-content VersionBundle R2 result");
        assert_eq!(
            leaf_terminal.outcome(),
            ReconciliationTerminalOutcome::Failed
        );
        assert_eq!(
            component_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert!(leaf_terminal.quarantine_checkpoint().is_empty());
        assert!(component_terminal.quarantine_checkpoint().is_empty());
        for (entry, attempt) in [leaf, component] {
            assert_eq!(
                state
                    .failure_memory()
                    .get(&reconciliation_attempt_key(attempt))
                    .and_then(|memory| memory.reconciliation_terminal().cloned()),
                entry.reconciliation_terminal().cloned()
            );
        }
        let foreground = integrity_foreground
            .as_ref()
            .expect("successful VersionBundle launch retains foreground");
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let postcheck =
            sense_integrity_tier1(&state, foreground, &lifecycle, &root.join("library"))
                .await
                .expect("wrong-content VersionBundle Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        assert_user_owned_launch_sentinels(&user_owned);
        drop((postcheck, lifecycle));

        let _ = state.sessions().kill(session_id).await;
        drop(integrity_foreground);
        state
            .close_known_good_inventories()
            .await
            .expect("close VersionBundle known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close VersionBundle instance registry");
        state
            .journals()
            .close()
            .await
            .expect("close VersionBundle journal");
        state
            .failure_memory()
            .close()
            .await
            .expect("close VersionBundle memory");
        drop((producer, state));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dropping_outer_launch_owner_keeps_owned_repair_settlement_alive() {
        let root = unique_test_dir("owned-launch-repair-settlement");
        let state = test_app_state(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim outer launch producer");
        let foreground = state
            .register_integrity_foreground()
            .expect("register repair integrity foreground")
            .wait_for_settlement()
            .await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_settled = settled.clone();
        let outer = tokio::spawn(async move {
            let _repair = producer.claim_child().spawn_joinable(async move {
                let _foreground = foreground;
                let _ = started_tx.send(());
                let _ = finish_rx.await;
                child_settled.store(true, std::sync::atomic::Ordering::Release);
            });
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("owned repair child start deadline")
            .expect("owned repair child started");
        outer.abort();
        let _ = outer.await;
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "repair child must retain lifecycle ownership after launch cancellation"
        );
        finish_tx.send(()).expect("release repair settlement");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !settled.load(std::sync::atomic::Ordering::Acquire)
                || !state.subscribe_integrity_idle().borrow().is_stably_idle()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned repair settlement deadline");
        let _ = fs::remove_dir_all(root);
    }

    fn empty_guardian_summary(mode: axial_launcher::GuardianMode) -> GuardianSummary {
        guardian_summary_for_test(
            mode,
            GuardianSummaryDecision::Allowed,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn launch_loop_caps_a_prepare_directive_that_never_marks_itself_applied() {
        let root = unique_test_dir("launch-recovery-cap");
        let state = test_app_state(&root);
        let session_id = "launch-recovery-cap";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let forced_failure = Arc::new(ForcedPrepareFailure::default());
        let control = LaunchLoopControl {
            forced_prepare_failure: Some(forced_failure.clone()),
            ..LaunchLoopControl::default()
        };
        let producer = state.try_claim_producer().expect("claim launch producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let (integrity_foreground, _update_admission, task) =
            LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner_with_control(
                state.clone(),
                task,
                &producer,
                &mut integrity_foreground,
                &control,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "launch recovery loop stalled after {} forced failures",
                forced_failure
                    .observed
                    .load(std::sync::atomic::Ordering::SeqCst)
            )
        });
        let error = match result {
            Ok(_) => panic!("non-applying recovery must fail at the cap"),
            Err(error) => error,
        };

        assert!(
            integrity_foreground.is_some(),
            "preboot launch failure must return foreground ownership to terminalization"
        );
        drop(integrity_foreground);
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());

        assert_eq!(
            forced_failure
                .observed
                .load(std::sync::atomic::Ordering::SeqCst),
            MAX_RECOVERY_ATTEMPTS + 1
        );
        assert_eq!(
            forced_failure
                .capped_message
                .lock()
                .expect("forced prepare failure lock")
                .as_deref(),
            Some("forced prepare failure 4")
        );
        let final_outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: crate::guardian::GuardianMode::Managed,
            failure_class: LaunchFailureClass::JavaRuntimeMismatch,
            public_error: "forced prepare failure 4",
            requested_java_present: true,
            explicit_java_override_present: true,
            explicit_jvm_args_present: false,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });
        assert_eq!(error.message, final_outcome.user_outcome.summary());
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("failed launch session");
        assert_eq!(
            record
                .stages
                .iter()
                .flat_map(|stage| &stage.evidence)
                .filter(|evidence| {
                    evidence.id == "guardian_launch_safety_decision"
                        && evidence.system == "guardian"
                })
                .count(),
            1
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_foreground_releases_only_after_success_metadata_settles() {
        let root = unique_test_dir("launch-foreground-success-metadata");
        let state = test_app_state(&root);
        let session_id = "launch-foreground-success-metadata";
        let registered = state
            .instances()
            .insert_for_test("Launch metadata gate".to_string(), "1.21.1".to_string())
            .expect("register launch instance");
        let mut record = test_record(session_id);
        record.instance_id = registered.id.clone();
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert metadata-gated session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe metadata-gated session");
        let java_path = write_delayed_boot_launch_fixture(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim metadata-gated producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, &registered.id);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let registry = state
            .instances()
            .acquire_mutation()
            .await
            .expect("hold launch metadata registry gate");
        assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

        let launch = tokio::spawn(launch_session(state.clone(), task, producer));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let LaunchEvent::Status(status) =
                    events.recv().await.expect("metadata-gated launch event")
                    && status.state == "running"
                {
                    break;
                }
            }
        })
        .await
        .expect("launch reaches running status");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.instance_lifecycle_is_held(&registered.id).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("launch metadata holds instance lifecycle");
        assert!(
            !launch.is_finished(),
            "launch must wait for metadata commit"
        );
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "foreground lease must remain live while metadata is blocked"
        );

        drop(registry);
        let launched = tokio::time::timeout(Duration::from_secs(5), launch)
            .await
            .expect("launch metadata settles")
            .expect("launch owner")
            .unwrap_or_else(|error| panic!("metadata-gated launch failed: {}", error.message));
        assert_eq!(launched.session_id, session_id);
        let stored = state
            .instances()
            .get(&registered.id)
            .expect("launch instance remains");
        assert_eq!(stored.last_played_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(
            state.instances().last_instance_id().as_deref(),
            Some(registered.id.as_str())
        );
        let mut integrity_idle = state.subscribe_integrity_idle();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if integrity_idle.borrow_and_update().is_stably_idle() {
                    break;
                }
                integrity_idle
                    .changed()
                    .await
                    .expect("integrity activity remains observable");
            }
        })
        .await
        .expect("metadata and witness foreground settle");
        let _ = state.sessions().kill(session_id).await;
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn out_of_memory_startup_exit_persists_bounded_proof_and_failure_memory() {
        let root = unique_test_dir("launch-out-of-memory-e2e");
        let paths = test_paths(&root);
        let session_id = "launch-out-of-memory-e2e";
        let java_path = write_out_of_memory_launch_fixture(&root);
        let instance = test_fabric_crash_instance(&java_path, 1024);
        let state = test_fabric_crash_app_state(&root, &instance);
        assert_scanner_recognizes_fabric_crash_install(&state);
        let producer = state.try_claim_producer().expect("claim OOM producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, CRASH_E2E_INSTANCE_ID);
        align_fabric_crash_task(&mut task, &java_path);
        task.instance.max_memory_mb = 1024;
        task.intent.max_memory_mb = 1024;
        task.intent.game_dir = Some(state.instances().game_dir(&task.instance.id));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let (integrity_foreground, _update_admission, task) =
            LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);
        let mut session = test_record(session_id);
        session.instance_id = CRASH_E2E_INSTANCE_ID.to_string();
        session.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert OOM session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe to OOM session");
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner(state.clone(), task, &producer, &mut integrity_foreground),
        )
        .await
        .expect("OOM launch deadline");
        let error = match result {
            Ok(_) => panic!("OOM launch must fail"),
            Err(error) => error,
        };
        drop(integrity_foreground);

        assert_eq!(error.message, "Guardian blocked launch startup.");
        assert!(error.message.chars().count() <= 180);
        let guardian = error.guardian.as_ref().expect("OOM Guardian summary");
        assert_eq!(
            guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Blocked
        );
        assert_eq!(guardian.message(), Some(error.message.as_str()));
        assert!(guardian.details().iter().any(|detail| {
            detail == "Minecraft exited before startup completed after running out of memory."
        }));
        assert!(guardian.guidance().iter().any(|detail| {
            detail
                == "Review the instance memory allocation and close memory-heavy apps before retrying."
        }));

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal OOM session");
        assert_eq!(record.state, LaunchState::Exited);
        assert_eq!(
            record.failure.as_ref().map(|failure| failure.class),
            Some(LaunchFailureClass::OutOfMemory)
        );
        assert_eq!(
            record
                .failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref()),
            Some("Guardian blocked launch startup.")
        );
        assert_eq!(
            record.outcome.as_ref().expect("OOM session outcome").reason,
            LaunchSessionExitReason::StartupFailed
        );
        let crash_evidence = record.crash_evidence.as_ref().expect("OOM crash evidence");
        assert_eq!(
            crash_evidence.source,
            axial_launcher::CrashArtifactKind::MinecraftCrashReport
        );
        assert!(crash_evidence.names_out_of_memory);

        let status_payload = serde_json::json!(
            super::super::reports::launch_status(&state, session_id)
                .await
                .expect("OOM status payload")
        );
        assert_eq!(status_payload["failure_class"], "out_of_memory");
        assert_eq!(
            status_payload["failure_detail"],
            "Guardian blocked launch startup."
        );
        assert_eq!(status_payload["guardian"]["decision"], "blocked");
        assert_eq!(
            status_payload["crash_evidence"]["exception_class"],
            "java.lang.OutOfMemoryError"
        );

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("OOM proof persisted");
        assert_eq!(proof.outcome, "failed");
        assert_eq!(proof.failure_class.as_deref(), Some("out_of_memory"));
        assert_eq!(
            proof
                .session_outcome
                .as_ref()
                .expect("OOM proof outcome")
                .reason,
            LaunchSessionExitReason::StartupFailed
        );
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert!(
            proof
                .crash_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.names_out_of_memory)
        );
        let report_payload = super::super::reports::launch_report_payload(&state, session_id)
            .expect("OOM report payload");
        assert_eq!(report_payload["failure_class"], "out_of_memory");

        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_out_of_memory_observation(&memory[0], CRASH_E2E_INSTANCE_ID);
        state
            .failure_memory()
            .flush()
            .await
            .expect("flush OOM failure memory");
        let memory_json = fs::read_to_string(failure_memory_path(&paths))
            .expect("read persisted OOM failure memory");
        let persisted = FailureMemorySnapshot::from_json(&memory_json)
            .expect("strict persisted OOM failure memory");
        assert_eq!(persisted.entries.len(), 1);
        assert_out_of_memory_observation(&persisted.entries[0], CRASH_E2E_INSTANCE_ID);
        let preflight = super::super::session::prepare_launch_preflight(
            &state,
            CRASH_E2E_INSTANCE_ID.to_string(),
        )
        .await
        .expect("prepare next preflight after OOM crash");
        assert_eq!(preflight.status, "ready");
        assert!(
            preflight.readiness.launchable,
            "Fabric OOM preflight readiness: {:?}",
            preflight.readiness.reasons
        );
        assert_eq!(
            preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        let recent_failure = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("OOM crash memory reaches actual next preflight");
        assert!(
            recent_failure
                .fields
                .iter()
                .any(|field| { field.key == "failure_class" && field.value == "out_of_memory" })
        );
        assert!(
            preflight.guardian.details().iter().any(|detail| {
                detail.contains("out-of-memory crash") && detail.contains("today")
            })
        );
        assert_oom_preflight_guidance(&preflight, recent_failure);
        let low_end_preflight =
            super::super::session::prepare_launch_preflight_with_memory_profile_for_test(
                &state,
                CRASH_E2E_INSTANCE_ID.to_string(),
                super::super::session::LaunchPreflightMemoryProfile {
                    host_total_memory_mb: Some(4096),
                    host_available_memory_mb: Some(3072),
                    host_used_memory_mb: Some(1024),
                    launcher_process_memory_mb: Some(128),
                },
            )
            .await
            .expect("prepare low-end preflight after Fabric OOM crash");
        assert!(low_end_preflight.readiness.launchable);
        assert_eq!(
            low_end_preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        assert_eq!(low_end_preflight.memory.max_memory_mb, 1024);
        assert_eq!(
            low_end_preflight
                .resource_budget
                .estimated_remaining_memory_mb,
            Some(3072)
        );
        assert!(!low_end_preflight.resource_budget.memory_pressure);
        let low_end_failure = low_end_preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("low-end OOM memory fact");
        assert_eq!(
            guardian_fact_field(low_end_failure, "current_memory_mb"),
            Some("1024")
        );
        assert_eq!(
            guardian_fact_field(low_end_failure, "suggested_memory_mb"),
            Some("2048")
        );
        assert!(
            low_end_preflight.guardian.details().iter().any(|detail| {
                detail.contains("out-of-memory crash") && detail.contains("today")
            })
        );
        assert!(low_end_preflight.guardian.guidance().iter().any(|guidance| {
            guidance
                == "Increase this instance's maximum memory from 1024 MB to 2048 MB before relaunching."
        }));
        assert_eq!(state.sessions().active_session_count().await, 0);
        assert!(
            !state
                .sessions()
                .has_active_instance(CRASH_E2E_INSTANCE_ID)
                .await
        );
        let original_session = state
            .sessions()
            .get(session_id)
            .await
            .expect("original OOM session remains");
        assert_eq!(original_session.state, LaunchState::Exited);
        assert_eq!(original_session.version_id, CRASH_E2E_FABRIC_VERSION_ID);
        let preflight_json = serde_json::to_string(&preflight).expect("OOM preflight json");

        let mut event_payloads = String::new();
        while let Ok(event) = events.try_recv() {
            match event {
                LaunchEvent::Status(status) => event_payloads.push_str(
                    &serde_json::to_string(&super::super::reports::public_launch_status(&status))
                        .expect("serialize public OOM status event"),
                ),
                LaunchEvent::Log(log) => event_payloads.push_str(
                    &serde_json::to_string(&log).expect("serialize public OOM log event"),
                ),
                LaunchEvent::ProcessSettled { .. } => {}
            }
        }
        let status_json = status_payload.to_string();
        let report_json = report_payload.to_string();
        for payload in [
            error.message.as_str(),
            status_json.as_str(),
            report_json.as_str(),
            memory_json.as_str(),
            event_payloads.as_str(),
            preflight_json.as_str(),
        ] {
            assert_no_out_of_memory_sensitive_decoys(payload);
            assert!(!payload.contains(&java_path));
        }

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_boot_out_of_memory_updates_guardian_proof_and_failure_memory() {
        let root = unique_test_dir("launch-post-boot-out-of-memory-e2e");
        let paths = test_paths(&root);
        let state = test_app_state(&root);
        let session_id = "launch-post-boot-out-of-memory-e2e";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert post-boot OOM session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe to post-boot OOM session");
        let java_path = write_post_boot_out_of_memory_launch_fixture(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim post-boot OOM producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session(state.clone(), task, producer),
        )
        .await
        .expect("post-boot OOM launch deadline")
        .unwrap_or_else(|error| panic!("launch must reach running before OOM: {}", error.message));
        assert_eq!(launched.session_id, session_id);

        let terminal_status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.expect("post-boot OOM event") {
                    LaunchEvent::Status(status)
                        if status.failure_detail.as_deref()
                            == Some("Minecraft stopped after running out of memory.") =>
                    {
                        break status;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("post-boot OOM observer deadline");
        assert_eq!(terminal_status.state, "exited");
        assert_eq!(
            terminal_status.failure_class.as_deref(),
            Some("out_of_memory")
        );
        assert_eq!(
            terminal_status
                .outcome
                .as_ref()
                .expect("post-boot OOM outcome")
                .reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            terminal_status
                .guardian
                .as_ref()
                .and_then(|guardian| guardian.get("decision")),
            Some(&serde_json::json!("warned"))
        );
        assert!(
            terminal_status
                .crash_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.names_out_of_memory)
        );
        assert!(
            state.launch_reports().load(session_id).is_some(),
            "proof must be durable before the terminal event is visible"
        );
        assert_eq!(
            state.failure_memory().list().len(),
            1,
            "failure memory must be updated before the terminal event is visible"
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.sessions().retention_hold_count(session_id).await == Some(0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-boot OOM observer settlement");

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("post-boot OOM record");
        assert!(record.boot_completed_at_ms.is_some());
        assert_eq!(
            record.failure.as_ref().map(|failure| failure.class),
            Some(LaunchFailureClass::OutOfMemory)
        );
        assert_eq!(
            record
                .outcome
                .as_ref()
                .expect("post-boot record outcome")
                .reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            record
                .crash_evidence
                .as_ref()
                .and_then(|evidence| evidence.exception_class.as_ref())
                .map(|exception| exception.as_str()),
            Some("java.lang.OutOfMemoryError")
        );
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("post-boot OOM proof");
        assert_eq!(proof.outcome, "failed");
        assert_eq!(proof.failure_class.as_deref(), Some("out_of_memory"));
        assert_eq!(
            proof
                .session_outcome
                .as_ref()
                .expect("post-boot proof outcome")
                .reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some("Minecraft stopped after running out of memory.")
        );
        assert!(
            proof
                .crash_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.names_out_of_memory)
        );

        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_out_of_memory_observation(&memory[0], "instance");
        state
            .failure_memory()
            .flush()
            .await
            .expect("flush post-boot OOM memory");
        let memory_json =
            fs::read_to_string(failure_memory_path(&paths)).expect("read post-boot OOM memory");
        let report_json = super::super::reports::launch_report_payload(&state, session_id)
            .expect("post-boot OOM report payload")
            .to_string();
        let status_json = serde_json::to_string(&terminal_status)
            .expect("serialize post-boot OOM terminal status");
        for payload in [
            memory_json.as_str(),
            report_json.as_str(),
            status_json.as_str(),
        ] {
            assert_no_out_of_memory_sensitive_decoys(payload);
        }

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_settlement_winning_boot_to_running_handoff_finishes_once() {
        let root = unique_test_dir("launch-boot-running-settlement-race");
        let state = test_app_state(&root);
        let session_id = "launch-boot-running-settlement-race";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert handoff race session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe handoff race session");
        let java_path = write_post_boot_out_of_memory_launch_fixture(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim handoff race producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let (barrier, reached, release) = LaunchLoopBarrier::new();
        let launch = tokio::spawn(launch_session_with_control(
            state.clone(),
            task,
            producer,
            LaunchLoopControl {
                before_running_handoff: Some(barrier),
                ..LaunchLoopControl::default()
            },
        ));

        tokio::time::timeout(Duration::from_secs(5), reached)
            .await
            .expect("running handoff barrier deadline")
            .expect("running handoff barrier reached");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state
                    .sessions()
                    .get(session_id)
                    .await
                    .is_some_and(|record| record.state == LaunchState::Settling)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("process settlement wins running handoff");
        release.send(()).expect("release running handoff");

        let launched = tokio::time::timeout(Duration::from_secs(5), launch)
            .await
            .expect("handoff race launch deadline")
            .expect("handoff race launch owner")
            .unwrap_or_else(|error| {
                panic!("settling handoff must remain successful: {}", error.message)
            });
        assert_eq!(launched.session_id, session_id);

        let mut running_statuses = 0;
        let mut settling_statuses = 0;
        let mut terminal_statuses = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let LaunchEvent::Status(status) =
                    events.recv().await.expect("handoff race event")
                {
                    running_statuses += usize::from(status.state == "running");
                    settling_statuses += usize::from(status.state == "settling");
                    terminal_statuses +=
                        usize::from(matches!(status.state.as_str(), "failed" | "exited"));
                    if terminal_statuses == 1 {
                        break;
                    }
                }
            }
        })
        .await
        .expect("handoff race terminal deadline");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.sessions().retention_hold_count(session_id).await == Some(0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("handoff race observer settlement");
        while let Ok(event) = events.try_recv() {
            if let LaunchEvent::Status(status) = event {
                running_statuses += usize::from(status.state == "running");
                settling_statuses += usize::from(status.state == "settling");
                terminal_statuses +=
                    usize::from(matches!(status.state.as_str(), "failed" | "exited"));
            }
        }
        assert_eq!(
            running_statuses, 1,
            "only the boot running state may publish"
        );
        assert_eq!(settling_statuses, 1, "settling must publish once");
        assert_eq!(terminal_statuses, 1, "settlement must publish once");
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("handoff race record");
        assert_eq!(record.state, LaunchState::Exited);
        assert_eq!(
            record.outcome.as_ref().map(|outcome| outcome.reason),
            Some(LaunchSessionExitReason::CrashedAfterBoot)
        );
        assert!(state.launch_reports().load(session_id).is_some());
        assert_eq!(state.failure_memory().list().len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn user_stop_during_running_handoff_short_circuits_guardian_failure_handling() {
        let root = unique_test_dir("launch-startup-user-stop-race");
        let state = test_app_state(&root);
        let session_id = "launch-startup-user-stop-race";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert startup stop session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe startup stop session");
        let java_path = write_delayed_boot_launch_fixture(&root);
        let launch_producer = state
            .try_claim_producer()
            .expect("claim startup stop launch producer");
        let stop_producer = state
            .try_claim_producer()
            .expect("claim startup stop command producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let (barrier, reached, release) = LaunchLoopBarrier::new();
        let launch = tokio::spawn(launch_session_with_control(
            state.clone(),
            task,
            launch_producer,
            LaunchLoopControl {
                before_running_handoff: Some(barrier),
                ..LaunchLoopControl::default()
            },
        ));

        tokio::time::timeout(Duration::from_secs(5), reached)
            .await
            .expect("running handoff stop barrier deadline")
            .expect("running handoff stop barrier reached");
        super::super::reports::stop_launch_session(&state, session_id, &stop_producer)
            .await
            .expect("stop startup process");
        release.send(()).expect("release running handoff stop");

        let result = tokio::time::timeout(Duration::from_secs(5), launch)
            .await
            .expect("startup stop launch deadline")
            .expect("startup stop launch owner");
        let error = match result {
            Ok(_) => panic!("startup stop must not report launch success"),
            Err(error) => error,
        };
        assert_eq!(error.message, "Launch stopped by user.");

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("startup stop record");
        assert_eq!(record.state, LaunchState::Exited);
        assert_eq!(
            record.outcome.as_ref().map(|outcome| outcome.reason),
            Some(LaunchSessionExitReason::LauncherStopped)
        );
        assert!(record.failure.is_none());
        assert!(state.failure_memory().list().is_empty());
        assert_eq!(
            state
                .launch_reports()
                .load(session_id)
                .expect("startup stop proof")
                .outcome,
            "stopped"
        );
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );

        let mut terminal_statuses = 0;
        let mut guardian_failures = 0;
        while let Ok(event) = events.try_recv() {
            if let LaunchEvent::Status(status) = event {
                terminal_statuses +=
                    usize::from(matches!(status.state.as_str(), "failed" | "exited"));
                guardian_failures += usize::from(status.failure_class.is_some());
            }
        }
        assert_eq!(terminal_statuses, 1, "user stop must publish once");
        assert_eq!(guardian_failures, 0, "user stop is not a Guardian failure");

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_boot_mod_crash_settles_guardian_copy_proof_and_failure_memory() {
        let root = unique_test_dir("launch-post-boot-mod-crash-e2e");
        let paths = test_paths(&root);
        let session_id = "launch-post-boot-mod-crash-e2e";
        let java_path = write_post_boot_mod_crash_launch_fixture(&root);
        let instance = test_fabric_crash_instance(&java_path, 4096);
        let state = test_fabric_crash_app_state(&root, &instance);
        assert_scanner_recognizes_fabric_crash_install(&state);
        let producer = state
            .try_claim_producer()
            .expect("claim mod crash producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, CRASH_E2E_INSTANCE_ID);
        align_fabric_crash_task(&mut task, &java_path);
        task.intent.game_dir = Some(state.instances().game_dir(&task.instance.id));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let mut session = test_record(session_id);
        session.instance_id = CRASH_E2E_INSTANCE_ID.to_string();
        session.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert mod crash session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe mod crash session");
        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session(state.clone(), task, producer),
        )
        .await
        .expect("mod crash launch deadline")
        .unwrap_or_else(|error| panic!("launch must reach running: {}", error.message));
        assert_eq!(launched.session_id, session_id);

        let (terminal, event_payloads) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut event_payloads = String::new();
            loop {
                match events.recv().await.expect("mod crash event") {
                    LaunchEvent::Status(status) => {
                        event_payloads.push_str(
                            &serde_json::to_string(&super::super::reports::public_launch_status(
                                &status,
                            ))
                            .expect("serialize public mod crash status event"),
                        );
                        if status.state == "exited" {
                            break (status, event_payloads);
                        }
                    }
                    LaunchEvent::Log(log) => event_payloads.push_str(
                        &serde_json::to_string(&log).expect("serialize public mod crash log event"),
                    ),
                    LaunchEvent::ProcessSettled { .. } => {}
                }
            }
        })
        .await
        .expect("mod crash terminal deadline");
        assert_eq!(
            terminal.outcome.as_ref().expect("mod crash outcome").reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        let evidence = terminal
            .crash_evidence
            .as_ref()
            .expect("mod crash evidence");
        assert_eq!(evidence.suspected_mods.len(), 1);
        assert_eq!(evidence.suspected_mods[0].name.as_str(), "Example Machines");
        assert!(terminal.guardian.is_some());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.sessions().retention_hold_count(session_id).await == Some(0)
                    && state
                        .launch_reports()
                        .load(session_id)
                        .is_some_and(|proof| proof.outcome == "failed")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mod crash proof settlement");

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("mod crash proof");
        let proof_evidence = proof
            .crash_evidence
            .as_ref()
            .expect("persisted mod crash evidence");
        assert_eq!(proof_evidence.suspected_mods.len(), 1);
        assert_eq!(
            proof_evidence.suspected_mods[0]
                .version
                .as_ref()
                .map(|version| version.as_str()),
            Some("3.2.1")
        );
        assert_eq!(proof.failure_class.as_deref(), Some("mod_attributed_crash"));
        assert!(
            proof
                .failure_detail
                .as_deref()
                .is_some_and(|detail| detail.contains("Example Machines"))
        );
        let status_payload = serde_json::json!(
            super::super::reports::launch_status(&state, session_id)
                .await
                .expect("settled mod crash status payload")
        );
        assert_eq!(
            status_payload["crash_evidence"]["suspected_mods"][0]["name"],
            "Example Machines"
        );
        assert_eq!(status_payload["failure_class"], "mod_attributed_crash");
        assert!(
            status_payload["failure_detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("Example Machines"))
        );
        let proof_json = serde_json::to_string(&proof).expect("serialize mod crash proof");
        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].diagnosis_id.as_str(), "mod_attributed_crash");
        assert_eq!(memory[0].target.id, CRASH_E2E_INSTANCE_ID);
        assert_eq!(memory[0].occurrence_count, 1);
        state
            .failure_memory()
            .flush()
            .await
            .expect("flush mod crash failure memory");
        let memory_json = fs::read_to_string(failure_memory_path(&paths))
            .expect("read persisted mod crash failure memory");
        let persisted = FailureMemorySnapshot::from_json(&memory_json)
            .expect("strict persisted mod crash failure memory");
        assert_eq!(persisted.entries, memory);
        let preflight = super::super::session::prepare_launch_preflight(
            &state,
            CRASH_E2E_INSTANCE_ID.to_string(),
        )
        .await
        .expect("prepare next preflight after mod crash");
        assert_eq!(preflight.status, "ready");
        assert!(preflight.readiness.launchable);
        assert_eq!(
            preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        let recent_failure = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("mod crash memory reaches actual next preflight");
        assert!(recent_failure.fields.iter().any(|field| {
            field.key == "failure_class" && field.value == "mod_attributed_crash"
        }));
        assert!(
            preflight.guardian.details().iter().any(|detail| {
                detail.contains("mod-attributed crash") && detail.contains("today")
            })
        );
        assert!(preflight.guardian.guidance().iter().any(|guidance| {
            guidance
                == "Review recently changed mods and disable the suspected mod before relaunching."
        }));
        let normal_preflight =
            super::super::session::prepare_launch_preflight_with_memory_profile_for_test(
                &state,
                CRASH_E2E_INSTANCE_ID.to_string(),
                super::super::session::LaunchPreflightMemoryProfile {
                    host_total_memory_mb: Some(16_384),
                    host_available_memory_mb: Some(12_288),
                    host_used_memory_mb: Some(4096),
                    launcher_process_memory_mb: Some(256),
                },
            )
            .await
            .expect("prepare normal-host preflight after Fabric mod crash");
        assert!(normal_preflight.readiness.launchable);
        assert_eq!(
            normal_preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        assert_eq!(normal_preflight.memory.max_memory_mb, 4096);
        assert_eq!(
            normal_preflight
                .resource_budget
                .estimated_remaining_memory_mb,
            Some(12_288)
        );
        assert!(!normal_preflight.resource_budget.memory_pressure);
        let normal_failure = normal_preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("normal-host mod crash memory fact");
        assert_eq!(
            guardian_fact_field(normal_failure, "failure_class"),
            Some("mod_attributed_crash")
        );
        assert!(
            normal_preflight.guardian.details().iter().any(|detail| {
                detail.contains("mod-attributed crash") && detail.contains("today")
            })
        );
        assert!(normal_preflight.guardian.guidance().iter().any(|guidance| {
            guidance
                == "Review recently changed mods and disable the suspected mod before relaunching."
        }));
        assert_eq!(state.sessions().active_session_count().await, 0);
        assert!(
            !state
                .sessions()
                .has_active_instance(CRASH_E2E_INSTANCE_ID)
                .await
        );
        let original_session = state
            .sessions()
            .get(session_id)
            .await
            .expect("original mod crash session remains");
        assert_eq!(original_session.state, LaunchState::Exited);
        assert_eq!(original_session.version_id, CRASH_E2E_FABRIC_VERSION_ID);
        let preflight_json = serde_json::to_string(&preflight).expect("mod preflight json");
        let status_json = status_payload.to_string();
        for payload in [
            status_json.as_str(),
            proof_json.as_str(),
            memory_json.as_str(),
            event_payloads.as_str(),
            preflight_json.as_str(),
        ] {
            for private in ["/home/alice", "raw-secret-token", "-Duser.home"] {
                assert!(!payload.contains(private));
            }
            assert!(!payload.contains(&java_path));
        }

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_launch_handoff_releases_terminal_observer_hold() {
        let root = unique_test_dir("cancelled-terminal-observer-handoff");
        let state = test_app_state(&root);
        let session_id = "cancelled-terminal-observer-handoff";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert observer handoff session");
        let (generation, events) = state
            .sessions()
            .subscribe_terminal_observation(session_id)
            .await
            .expect("subscribe observer handoff");
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        drop(handoff_tx);
        let producer = state
            .try_claim_producer()
            .expect("claim observer task producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let proof_context = LaunchProofContext::from_intent(&task.intent, &task.performance_mode);
        drop(task);

        own_terminal_observation(
            state.clone(),
            session_id.to_string(),
            "instance".to_string(),
            GuardianMode::Managed,
            "2026-01-01T00:00:00.000Z".to_string(),
            proof_context,
            generation,
            empty_guardian_summary(axial_launcher::GuardianMode::Managed),
            producer,
            events,
            handoff_rx,
        )
        .await;

        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );
        assert!(state.failure_memory().list().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn terminal_observer_ignores_queued_recovery_event_after_retry_started() {
        let root = unique_test_dir("stale-recovery-observer-event");
        let state = test_app_state(&root);
        let session_id = "stale-recovery-observer-event";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert recovery-event session");
        let (generation, events) = state
            .sessions()
            .subscribe_terminal_observation(session_id)
            .await
            .expect("subscribe recovery-event observer");
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        let producer = state
            .try_claim_producer()
            .expect("claim recovery-event producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let proof_context = LaunchProofContext::from_intent(&task.intent, &task.performance_mode);
        drop(task);
        let observer_state = state.clone();
        let mut observer = tokio::spawn(own_terminal_observation(
            observer_state,
            session_id.to_string(),
            "instance".to_string(),
            GuardianMode::Managed,
            "2026-01-01T00:00:00.000Z".to_string(),
            proof_context,
            generation,
            empty_guardian_summary(axial_launcher::GuardianMode::Managed),
            producer,
            events,
            handoff_rx,
        ));

        emit_status(
            &state,
            session_id,
            LaunchState::Recovering,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            state
                .sessions()
                .begin_startup_recovery_retry(session_id)
                .await,
            "begin same-session startup retry"
        );
        emit_status(
            &state,
            session_id,
            LaunchState::Preparing,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            handoff_tx
                .send(TerminalObservationHandoff::Observe {
                    guardian: empty_guardian_summary(axial_launcher::GuardianMode::Managed),
                })
                .is_ok(),
            "handoff retry observer"
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut observer)
                .await
                .is_err(),
            "queued attempt-1 recovery event must not settle the retry session"
        );

        emit_status(
            &state,
            session_id,
            LaunchState::Exited,
            None,
            None,
            None,
            None,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), observer)
            .await
            .expect("fresh terminal observation deadline")
            .expect("fresh terminal observer task");

        assert_eq!(
            state
                .sessions()
                .get(session_id)
                .await
                .expect("fresh terminal session")
                .state,
            LaunchState::Exited
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn modern_plan_skips_repair_stage_full_asset_index_parse() {
        let root = unique_test_dir("modern-asset-index-guard");
        write_runner_asset_index(&root, "modern", r#"{"objects":"not-an-object-map"}"#);
        let plan = test_asset_launch_plan("modern", false);

        let outcome = repair_legacy_virtual_assets_before_launch(&root, &plan)
            .await
            .expect("modern asset repair guard");

        assert_eq!(outcome, LegacyVirtualAssetRepairOutcome::SkippedModern);
        assert_eq!(outcome.full_object_parse_attempts(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_plan_invokes_repair_stage_full_asset_index_parser() {
        let root = unique_test_dir("legacy-asset-index-guard");
        write_runner_asset_index(
            &root,
            "legacy",
            r#"{"objects":"not-an-object-map","map_to_resources":true}"#,
        );
        let plan = test_asset_launch_plan("legacy", true);

        assert!(matches!(
            repair_legacy_virtual_assets_before_launch(&root, &plan).await,
            Err(axial_minecraft::download::DownloadError::ParseVersion(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejected_launch_recovery_plan_finishes_terminal_and_persists_proof() {
        let root = unique_test_dir("rejected-launch-recovery-plan");
        let state = test_app_state(&root);
        let session_id = "rejected-launch-recovery-plan";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut guardian = empty_guardian_summary(axial_launcher::GuardianMode::Managed);
        let user_outcome = guardian_user_outcome_for_test(
            GuardianActionKind::Block,
            OperationPhase::Preparing,
            "Guardian blocked launch recovery planning.",
            &["The recovery directive could not be planned safely."],
            &[],
        );

        guardian = guardian_summary_with_blocked_outcome(&guardian, &user_outcome);
        let mut launch_completion_pending = true;
        let error = finish_launch_failure(
            &state,
            &state.try_claim_producer().expect("claim failure producer"),
            session_id,
            &mut launch_completion_pending,
            LaunchFailure {
                proof_context: None,
                class: LaunchFailureClass::Unknown,
                message: user_outcome.summary(),
                healing: None,
                guardian: Some(guardian.clone()),
                outcome: None,
            },
        )
        .await;

        assert_eq!(error.message, user_outcome.summary());
        assert_eq!(
            guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Blocked
        );
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal session");
        assert_eq!(record.state, LaunchState::Exited);
        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("proof persisted");
        assert_eq!(proof.session_id, session_id);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn nonretryable_journal_error_cannot_leave_a_queued_session_orphaned() {
        let root = unique_test_dir("nonretryable-launch-journal-error");
        let state = test_app_state(&root);
        let session_id = "nonretryable-launch-journal-error";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);

        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let mut update_admission = Some(
            state
                .try_admit_update_sensitive_operation()
                .expect("admit terminal update operation"),
        );
        let result = match terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
            &mut update_admission,
        )
        .await
        {
            LaunchTerminalizationDisposition::Complete(result)
            | LaunchTerminalizationDisposition::Retained(result)
            | LaunchTerminalizationDisposition::Settled(result) => result,
        };

        assert!(result.is_err());
        assert!(integrity_foreground.is_some());
        assert!(update_admission.is_some());
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal session");
        assert_eq!(record.state, LaunchState::Failed);
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_process_journal_error_settles_as_guardian_failure() {
        let root = unique_test_dir("spawned-launch-journal-error");
        let state = test_app_state(&root);
        let session_id = "spawned-launch-journal-error";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        state
            .sessions()
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn test process");
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);
        let expected_message = error.message.clone();

        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let mut update_admission = Some(
            state
                .try_admit_update_sensitive_operation()
                .expect("admit terminal update operation"),
        );
        let result = match terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
            &mut update_admission,
        )
        .await
        {
            LaunchTerminalizationDisposition::Complete(result)
            | LaunchTerminalizationDisposition::Retained(result)
            | LaunchTerminalizationDisposition::Settled(result) => result,
        };

        assert!(result.is_err());
        assert!(integrity_foreground.is_some());
        assert!(update_admission.is_some());
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal session");
        assert_eq!(record.state, LaunchState::Failed);
        assert_eq!(
            record
                .failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref()),
            Some(expected_message.as_str())
        );
        assert_ne!(
            record.outcome.expect("failure outcome").reason,
            LaunchSessionExitReason::LauncherStopped
        );
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_process_termination_retains_active_launch_until_confirmed_settlement() {
        let root = unique_test_dir("rejected-launch-failure-termination");
        let state = test_app_state_with_library(&root);
        let instance = state
            .instances()
            .insert_for_test("Termination rejection".to_string(), "1.21.1".to_string())
            .expect("add test instance");
        let session_id = "rejected-launch-failure-termination";
        let process_release = root.join("release-process");
        let mut record = test_record(session_id);
        record.instance_id = instance.id.clone();
        state
            .sessions()
            .insert(record.clone())
            .await
            .expect("insert session");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("while [ ! -f \"$1\" ]; do sleep 0.01; done")
            .arg("rejected-launch-failure-termination")
            .arg(&process_release);
        state
            .sessions()
            .start_process(record, command)
            .await
            .expect("spawn test process");
        assert!(
            state
                .sessions()
                .reject_next_process_start_kill(session_id)
                .await
        );
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);
        let expected_message = error.message.clone();

        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let mut update_admission = Some(
            state
                .try_admit_update_sensitive_operation()
                .expect("admit terminal update operation"),
        );
        let result = match terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
            &mut update_admission,
        )
        .await
        {
            LaunchTerminalizationDisposition::Complete(result)
            | LaunchTerminalizationDisposition::Retained(result)
            | LaunchTerminalizationDisposition::Settled(result) => result,
        };

        let returned_error = match result {
            Ok(_) => panic!("journal error must remain public"),
            Err(error) => error,
        };
        assert_eq!(returned_error.message, expected_message);
        assert!(integrity_foreground.is_none());
        assert!(update_admission.is_none());
        assert_eq!(
            state.try_begin_update_apply().unwrap_err(),
            UpdateApplyAdmissionError::ActiveOperations
        );
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "pending terminalization must retain foreground ownership"
        );
        let active = state
            .sessions()
            .get(session_id)
            .await
            .expect("active session");
        assert!(!matches!(
            active.state,
            LaunchState::Failed | LaunchState::Exited
        ));
        assert!(active.pid.is_some());
        assert!(state.sessions().has_active_instance(&instance.id).await);
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(1)
        );

        let conflict = match super::super::prepare_launch_session(
            &state,
            super::super::LaunchRequest {
                instance_id: instance.id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("a live retained child must block a second launch"),
            Err(error) => error,
        };
        assert_eq!(conflict.0, axum::http::StatusCode::CONFLICT);
        assert_eq!(
            conflict.1.0["error"],
            "instance already has an active session"
        );

        fs::write(&process_release, b"release").expect("release process naturally");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let settled = state
                    .sessions()
                    .get(session_id)
                    .await
                    .expect("settled session");
                if settled.state == LaunchState::Failed
                    && state.sessions().retention_hold_count(session_id).await == Some(0)
                    && state.subscribe_integrity_idle().borrow().is_stably_idle()
                    && state.try_begin_update_apply().is_ok()
                {
                    assert_eq!(
                        settled
                            .failure
                            .as_ref()
                            .and_then(|failure| failure.detail.as_deref()),
                        Some(expected_message.as_str())
                    );
                    assert_ne!(
                        settled.outcome.expect("failure outcome").reason,
                        LaunchSessionExitReason::LauncherStopped
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("deferred launch failure settlement");
        assert!(!state.sessions().has_active_instance(&instance.id).await);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unconfirmed_launch_failure_retains_foreground_until_exact_session_terminal() {
        let root = unique_test_dir("unconfirmed-launch-failure-foreground");
        let state = test_app_state(&root);
        let session_id = "unconfirmed-launch-failure-foreground";
        let mut record = test_record(session_id);
        record.pid = Some(42);
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert unconfirmed session");
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);
        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let mut update_admission = Some(
            state
                .try_admit_update_sensitive_operation()
                .expect("admit terminal update operation"),
        );

        let disposition = terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
            &mut update_admission,
        )
        .await;

        assert!(matches!(
            disposition,
            LaunchTerminalizationDisposition::Retained(Err(_))
        ));
        assert!(integrity_foreground.is_none());
        assert!(update_admission.is_none());
        assert_eq!(
            state.try_begin_update_apply().unwrap_err(),
            UpdateApplyAdmissionError::ActiveOperations
        );
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "unconfirmed terminalization must retain foreground ownership"
        );

        emit_status(
            &state,
            session_id,
            LaunchState::Failed,
            None,
            None,
            None,
            None,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !state.subscribe_integrity_idle().borrow().is_stably_idle()
                || state.try_begin_update_apply().is_err()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unconfirmed foreground release after exact terminal state");
        assert_eq!(
            state
                .sessions()
                .get(session_id)
                .await
                .expect("terminal session")
                .state,
            LaunchState::Failed
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unconfirmed_foreground_owner_drains_after_shutdown_session_settlement() {
        let root = unique_test_dir("unconfirmed-launch-failure-shutdown");
        let state = test_app_state(&root);
        let session_id = "unconfirmed-launch-failure-shutdown";
        let mut record = test_record(session_id);
        record.pid = Some(42);
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert unconfirmed session");
        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let mut update_admission = Some(
            state
                .try_admit_update_sensitive_operation()
                .expect("admit terminal update operation"),
        );

        let disposition = terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(guardian_journal_error(
                OperationJournalStoreError::MissingOperation,
            )),
            &mut integrity_foreground,
            &mut update_admission,
        )
        .await;

        assert!(matches!(
            disposition,
            LaunchTerminalizationDisposition::Retained(Err(_))
        ));
        assert!(integrity_foreground.is_none());
        drop(producer);
        tokio::time::timeout(Duration::from_secs(2), state.shutdown())
            .await
            .expect("shutdown must not deadlock on retained foreground")
            .expect("shutdown after exact session settlement");
        assert!(state.sessions().get(session_id).await.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_started_emits_once_while_completion_is_pending() {
        let root = unique_test_dir("launch-started-once");
        let state = test_app_state_with_telemetry(&root);
        let mut pending = false;

        emit_launch_started(&state, &mut pending, Some("fabric".to_string()));
        assert!(pending);

        let queued = state.telemetry().queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "launch_started");
        assert_eq!(queued[0]["properties"]["loader_key"], "fabric");

        emit_launch_started(&state, &mut pending, Some("neoforge".to_string()));
        let queued = state.telemetry().queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["properties"]["loader_key"], "fabric");

        emit_launch_completed(&state, &mut pending, TelemetryLaunchOutcome::Success);
        assert!(!pending);
        assert_eq!(state.telemetry().queue_len_for_test(), 2);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_launch_failure_emits_once_only_when_completion_is_pending() {
        let root = unique_test_dir("pending-launch-failure");
        let state = test_app_state_with_telemetry(&root);
        let mut pending = false;

        emit_pending_launch_failure(&state, &mut pending);
        assert_eq!(state.telemetry().queue_len_for_test(), 0);

        pending = true;
        emit_pending_launch_failure(&state, &mut pending);
        assert!(!pending);

        let queued = state.telemetry().queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "launch_completed");
        assert_eq!(queued[0]["properties"]["outcome"], "failure");

        emit_pending_launch_failure(&state, &mut pending);
        assert_eq!(state.telemetry().queue_len_for_test(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn p00_b08_contract_runner_transitions_from_preparing_to_starting() {
        let root = unique_test_dir("runner-stage-evidence");
        let state = test_app_state(&root);
        let session_id = "runner-stage-evidence";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe");

        emit_status(
            &state,
            session_id,
            LaunchState::Preparing,
            None,
            None,
            None,
            None,
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("preparing status")
            .expect("preparing status event");

        let command = vec![
            r"C:\Users\Alice\.jdks\java.exe".to_string(),
            "-cp".to_string(),
            "libraries".to_string(),
        ];
        state
            .sessions()
            .record_stage_evidence(
                session_id,
                launch_command_stage_evidence(true, command.len()),
            )
            .await;

        emit_status(
            &state,
            session_id,
            LaunchState::Starting,
            None,
            None,
            None,
            None,
        )
        .await;
        let status = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.expect("launch status event");
                if let LaunchEvent::Status(status) = event
                    && status.state == "starting"
                {
                    break status;
                }
            }
        })
        .await
        .expect("starting status");
        assert_eq!(status.state, "starting");
        let retired_stage = ["pre", "warming"].concat();
        assert!(
            status
                .stages
                .iter()
                .all(|stage| stage.stage != retired_stage)
        );
        let preparing_stage = status
            .stages
            .iter()
            .find(|stage| stage.stage == "preparing")
            .expect("preparing stage");
        assert!(
            preparing_stage
                .evidence
                .iter()
                .any(|evidence| evidence.id == "application_launch_command_prepared")
        );
        let status_json = serde_json::to_string(&status).expect("status json");
        assert_no_sensitive_stage_evidence(&status_json);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn p00_b08_contract_registered_artifact_diagnostic_reasons_are_closed_tokens() {
        let reasons = [
            RegisteredArtifactRepairFailureReason::EvidenceUnavailable,
            RegisteredArtifactRepairFailureReason::AuthorizationRejected,
            RegisteredArtifactRepairFailureReason::SessionNotCurrent,
            RegisteredArtifactRepairFailureReason::AdmissionRejected,
            RegisteredArtifactRepairFailureReason::ExecutionFailed,
            RegisteredArtifactRepairFailureReason::OwnerStopped,
        ];

        assert_eq!(
            reasons.map(RegisteredArtifactRepairFailureReason::as_str),
            [
                "evidence_unavailable",
                "authorization_rejected",
                "session_not_current",
                "admission_rejected",
                "execution_failed",
                "owner_stopped",
            ]
        );
        assert!(reasons.into_iter().all(|reason| {
            reason
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }));
        assert_eq!(
            LegacyVirtualAssetRepairOutcome::SkippedModern.label(),
            "skipped_modern"
        );
        assert_eq!(
            LegacyVirtualAssetRepairOutcome::RepairedLegacy.label(),
            "repaired_legacy"
        );
    }

    fn test_recovery_launch_instance() -> Instance {
        Instance {
            id: "instance".to_string(),
            name: "Recovery cap".to_string(),
            version_id: "1.21.1".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_played_at: String::new(),
            art_seed: 0,
            max_memory_mb: 4096,
            min_memory_mb: 1024,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: "managed".to_string(),
            extra_jvm_args: String::new(),
            auto_optimize: false,
            icon: String::new(),
            accent: String::new(),
            loader_key: String::new(),
            minecraft_version: "1.21.1".to_string(),
        }
    }

    async fn test_recovery_launch_task(
        state: &AppState,
        session_id: &str,
        root: &Path,
    ) -> super::super::session::LaunchSessionTask {
        let integrity_foreground = state
            .register_integrity_foreground()
            .expect("register test launch foreground")
            .wait_for_settlement()
            .await;
        super::super::session::LaunchSessionTask {
            update_admission: state
                .try_admit_update_sensitive_operation()
                .expect("admit test launch"),
            integrity_foreground,
            preflight_stage_evidence: crate::application::launch_preflight_stage_evidence(
                &crate::guardian::guardian_preflight_outcome(
                    crate::guardian::GuardianPreflightOutcomeRequest::new(
                        crate::guardian::GuardianMode::Managed,
                        &[],
                    ),
                ),
                "managed",
            ),
            session_id: axial_launcher::SessionId(session_id.to_string()),
            instance: test_recovery_launch_instance(),
            intent: LaunchIntent {
                library_dir: root.join("library"),
                version_id: "1.21.1".to_string(),
                target_version_id: "1.21.1".to_string(),
                loader: "vanilla".to_string(),
                is_modded: false,
                auth: LaunchAuthContext::offline("Player"),
                requested_java: "configured-java".to_string(),
                requested_preset: String::new(),
                extra_jvm_args: Vec::new(),
                max_memory_mb: 4096,
                min_memory_mb: 1024,
                resolution: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                game_dir: None,
                guardian: LaunchGuardianContext {
                    mode: axial_launcher::GuardianMode::Managed,
                    java_override_origin: Some(OverrideOrigin::Instance),
                    preset_override_origin: None,
                    raw_jvm_args_origin: None,
                },
                low_impact_startup: true,
            },
            performance_mode: "managed".to_string(),
            guardian: empty_guardian_summary(axial_launcher::GuardianMode::Managed),
            launched_at: "2026-01-01T00:00:00Z".to_string(),
            benchmark: None,
            resource_budget: None,
            java_probe_receipt: None,
        }
    }

    #[cfg(unix)]
    fn retarget_test_launch_task(
        task: &mut super::super::session::LaunchSessionTask,
        instance_id: &str,
    ) {
        task.instance.id = instance_id.to_string();
    }

    #[cfg(unix)]
    fn test_fabric_crash_instance(java_path: &str, max_memory_mb: i32) -> Instance {
        let mut instance = test_recovery_launch_instance();
        instance.id = CRASH_E2E_INSTANCE_ID.to_string();
        instance.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        instance.java_path = java_path.to_string();
        instance.max_memory_mb = max_memory_mb;
        instance
    }

    #[cfg(unix)]
    fn align_fabric_crash_task(
        task: &mut super::super::session::LaunchSessionTask,
        java_path: &str,
    ) {
        task.instance.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        task.instance.java_path = java_path.to_string();
        task.intent.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        task.intent.target_version_id = "1.21.1".to_string();
        task.intent.loader = "fabric".to_string();
        task.intent.is_modded = true;
        task.intent.requested_java = java_path.to_string();
    }

    #[cfg(unix)]
    fn assert_scanner_recognizes_fabric_crash_install(state: &AppState) {
        let operation = state
            .try_acquire_managed_library()
            .expect("Fabric crash fixture library operation");
        let version_report = axial_minecraft::scan_versions_report(operation.core())
            .expect("scan Fabric crash fixture");
        assert_eq!(
            version_report.state,
            axial_minecraft::VersionScanState::Ready
        );
        let fabric_version = version_report
            .versions
            .iter()
            .find(|version| version.id == CRASH_E2E_FABRIC_VERSION_ID)
            .expect("scanner-recognized Fabric crash fixture");
        assert!(fabric_version.installed);
        assert!(fabric_version.launchable);
        assert_eq!(fabric_version.inherits_from, "1.21.1");
        let fabric_loader = fabric_version
            .loader
            .as_ref()
            .expect("authoritative Fabric loader metadata");
        assert_eq!(
            fabric_loader.component_id,
            axial_minecraft::LoaderComponentId::Fabric
        );
        assert_eq!(
            fabric_loader.build_id,
            axial_minecraft::build_id_for(
                axial_minecraft::LoaderComponentId::Fabric,
                "1.21.1",
                "0.16.10"
            )
        );
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("load config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("load instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        })
    }

    fn test_libraries_recovery_app_state(
        root: &Path,
        instance_id: &str,
        repair_source: &str,
    ) -> (AppState, Arc<KnownGoodInventory>) {
        let state = test_registered_recovery_app_state(root, instance_id, "Libraries recovery");
        let library_dir = PathBuf::from(
            state
                .library_dir()
                .expect("Libraries recovery managed root"),
        );
        fs::create_dir_all(
            library_dir
                .join(MANAGED_LIBRARY_FIXTURE_PATH)
                .parent()
                .expect("Libraries recovery managed library parent"),
        )
        .expect("Libraries recovery managed library directory");
        let active_inventory = state.activate_known_good_inventory_for_test_with_identity(
            instance_id,
            KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
                root: TestKnownGoodRoot::Libraries,
                path: MANAGED_LIBRARY_FIXTURE_PATH
                    .strip_prefix("libraries/")
                    .expect("Libraries fixture inventory-relative path")
                    .to_string(),
                kind: KnownGoodArtifactKind::Library,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(MANAGED_LIBRARY_FIXTURE_BYTES)),
                    size: MANAGED_LIBRARY_FIXTURE_BYTES.len() as u64,
                },
            }])
            .expect("Libraries recovery known-good inventory")
            .with_test_standalone_leaf_repair_source(0, repair_source)
            .expect("Libraries recovery standalone leaf source"),
        );
        (state, active_inventory)
    }

    fn test_registered_recovery_app_state(
        root: &Path,
        instance_id: &str,
        instance_name: &str,
    ) -> AppState {
        let paths = test_paths(root);
        let library_dir = root.join("library");
        fs::create_dir_all(paths.instances_dir().join(instance_id))
            .expect("registered recovery instance directory");
        fs::create_dir_all(&library_dir).expect("registered recovery managed root");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                Arc::clone(&root_session),
                AppConfig {
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
            )
            .expect("configure registered recovery root"),
        );
        let mut instance = test_recovery_launch_instance();
        instance.id = instance_id.to_string();
        instance.name = instance_name.to_string();
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::new(vec![instance], instance_id.to_string(), Vec::new())
                    .expect("registered recovery registry snapshot"),
            )
            .expect("load registered recovery instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("registered recovery performance manager"),
            ),
            startup_warnings: Vec::new(),
        })
    }

    #[cfg(unix)]
    fn test_version_bundle_recovery_app_state(
        root: &Path,
        instance_id: &str,
    ) -> (AppState, PathBuf, Vec<u8>) {
        const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
        const LOG_ID: &str = "guardian-version-bundle.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";

        let paths = test_paths(root);
        let library_dir = root.join("library");
        fs::create_dir_all(paths.instances_dir().join(instance_id))
            .expect("VersionBundle recovery instance directory");
        let mut instance = test_recovery_launch_instance();
        instance.id = instance_id.to_string();
        instance.name = "VersionBundle recovery".to_string();
        let version_id = instance.version_id.clone();
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": version_id.as_str(),
            "type": "release",
            "mainClass": "org.axial.GuardianFixture"
        }))
        .expect("VersionBundle recovery metadata");
        let version_dir = library_dir.join("versions").join(&version_id);
        fs::create_dir_all(&version_dir).expect("VersionBundle recovery version directory");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            &version_json,
        )
        .expect("VersionBundle recovery metadata");
        let client_path = version_dir.join(format!("{version_id}.jar"));
        fs::write(&client_path, vec![7_u8; CLIENT_BYTES.len()])
            .expect("same-size wrong-content VersionBundle client");
        let log_path = library_dir.join("assets/log_configs").join(LOG_ID);
        fs::create_dir_all(log_path.parent().expect("VersionBundle log parent"))
            .expect("VersionBundle log directory");
        fs::write(&log_path, LOG_BYTES).expect("VersionBundle log config");

        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                Arc::clone(&root_session),
                AppConfig {
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
            )
            .expect("configure VersionBundle recovery root"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::new(vec![instance], instance_id.to_string(), Vec::new())
                    .expect("VersionBundle recovery registry snapshot"),
            )
            .expect("load VersionBundle recovery instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("VersionBundle recovery performance manager"),
            ),
            startup_warnings: Vec::new(),
        });
        state.activate_known_good_inventory_for_test(
            instance_id,
            KnownGoodInventory::from_test_entries([
                TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Versions,
                    path: format!("{version_id}/{version_id}.json"),
                    kind: KnownGoodArtifactKind::VersionMetadata,
                    integrity: TestKnownGoodIntegrity::Sha1 {
                        digest: format!("{:x}", Sha1::digest(&version_json)),
                        size: version_json.len() as u64,
                    },
                },
                TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Versions,
                    path: format!("{version_id}/{version_id}.jar"),
                    kind: KnownGoodArtifactKind::ClientJar,
                    integrity: TestKnownGoodIntegrity::Sha1 {
                        digest: format!("{:x}", Sha1::digest(CLIENT_BYTES)),
                        size: CLIENT_BYTES.len() as u64,
                    },
                },
                TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Assets,
                    path: format!("log_configs/{LOG_ID}"),
                    kind: KnownGoodArtifactKind::LogConfig,
                    integrity: TestKnownGoodIntegrity::Sha1 {
                        digest: format!("{:x}", Sha1::digest(LOG_BYTES)),
                        size: LOG_BYTES.len() as u64,
                    },
                },
            ])
            .expect("VersionBundle recovery inventory"),
        );
        (state, client_path, CLIENT_BYTES.to_vec())
    }

    fn write_user_owned_launch_sentinels(
        state: &AppState,
        instance_id: &str,
    ) -> Vec<(PathBuf, Vec<u8>)> {
        [
            ("saves/world/level.dat", b"world".as_slice()),
            ("mods/user.jar", b"mod".as_slice()),
            ("config/user.toml", b"config".as_slice()),
            ("resourcepacks/user.zip", b"resourcepack".as_slice()),
            ("custom/opaque/nested.bin", b"unknown-owned".as_slice()),
        ]
        .into_iter()
        .map(|(relative, contents)| {
            let path = state.instances().game_dir(instance_id).join(relative);
            fs::create_dir_all(path.parent().expect("user-owned launch sentinel parent"))
                .expect("user-owned launch sentinel directory");
            fs::write(&path, contents).expect("write user-owned launch sentinel");
            (path, contents.to_vec())
        })
        .collect()
    }

    fn assert_user_owned_launch_sentinels(sentinels: &[(PathBuf, Vec<u8>)]) {
        for (path, contents) in sentinels {
            assert_eq!(
                fs::read(path).expect("user-owned launch sentinel remains readable"),
                *contents
            );
        }
    }

    fn registered_artifact_repair_decision(
        operation_id: &str,
        target: TargetDescriptor,
    ) -> crate::guardian::GuardianDecision {
        crate::guardian::GuardianDecision::for_test(
            Some(OperationId::deterministic_test(operation_id)),
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            Some(crate::guardian::GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                crate::guardian::ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                    ownership: OwnershipClass::LauncherManaged,
                    confidence: crate::guardian::GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![GuardianActionKind::Repair],
                },
                vec![crate::guardian::GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

    #[cfg(unix)]
    fn test_fabric_crash_app_state(root: &Path, instance: &Instance) -> AppState {
        let paths = test_paths(root);
        fs::create_dir_all(paths.instances_dir().join(&instance.id))
            .expect("registered launch instance directory");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                Arc::clone(&root_session),
                AppConfig {
                    library_dir: root.join("library").to_string_lossy().to_string(),
                    ..AppConfig::default()
                },
            )
            .expect("configure registered launch library"),
        );
        let snapshot =
            InstanceRegistrySnapshot::new(vec![instance.clone()], instance.id.clone(), Vec::new())
                .expect("registered launch instance snapshot");
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), root_session, snapshot)
                .expect("load registered launch instance"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        });
        activate_fabric_crash_known_good(&state, root, &instance.id);
        state
    }

    #[cfg(unix)]
    fn activate_fabric_crash_known_good(state: &AppState, root: &Path, instance_id: &str) {
        use std::os::unix::fs::PermissionsExt;

        let library = root.join("library");
        let version_dir = library.join("versions").join(CRASH_E2E_FABRIC_VERSION_ID);
        let version_json = version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.json"));
        let client_jar = version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.jar"));
        let runtime_root = state
            .managed_runtime_cache()
            .component_root("java-runtime-delta")
            .expect("runtime root");
        let runtime_java = if cfg!(target_os = "macos") {
            runtime_root.join("jre.bundle/Contents/Home/bin/java")
        } else {
            runtime_root.join("bin/java")
        };
        fs::create_dir_all(runtime_java.parent().expect("runtime Java parent"))
            .expect("runtime Java directory");
        let runtime_java_bytes = b"java";
        fs::write(&runtime_java, runtime_java_bytes).expect("runtime Java");
        let mut permissions = fs::metadata(&runtime_java)
            .expect("runtime Java metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runtime_java, permissions).expect("runtime Java executable");
        let runtime_proof = runtime_root.join(".axial-runtime-manifest.json");
        let runtime_marker = runtime_root.join(".axial-ready");
        let runtime_java_relative = runtime_java
            .strip_prefix(&runtime_root)
            .expect("runtime Java relative path")
            .to_string_lossy()
            .replace('\\', "/");
        let runtime_manifest = serde_json::json!({
            "files": {
                runtime_java_relative.clone(): {
                    "type": "file",
                    "downloads": {
                        "raw": {
                            "url": "https://example.invalid/java",
                            "sha1": hex::encode(Sha1::digest(runtime_java_bytes)),
                            "size": runtime_java_bytes.len()
                        }
                    }
                }
            }
        });
        fs::write(
            &runtime_proof,
            serde_json::to_vec(&runtime_manifest).expect("runtime manifest JSON"),
        )
        .expect("runtime proof");
        fs::write(&runtime_marker, b"ready").expect("runtime marker");

        let runtime_kind = || TestKnownGoodRoot::ManagedRuntime {
            component: "java-runtime-delta".to_string(),
        };
        let file = |entry_root, path, kind, physical_path: &Path| TestKnownGoodEntry {
            root: entry_root,
            path,
            kind,
            integrity: TestKnownGoodIntegrity::File {
                size: fs::metadata(physical_path).expect("known-good file").len(),
            },
        };
        let library_entries = CRASH_E2E_FABRIC_LIBRARIES.iter().map(|(_, relative)| {
            file(
                TestKnownGoodRoot::Libraries,
                (*relative).to_string(),
                KnownGoodArtifactKind::Library,
                &library.join("libraries").join(relative),
            )
        });
        let mut entries = vec![
            file(
                TestKnownGoodRoot::Versions,
                format!("{CRASH_E2E_FABRIC_VERSION_ID}/{CRASH_E2E_FABRIC_VERSION_ID}.json"),
                KnownGoodArtifactKind::VersionMetadata,
                &version_json,
            ),
            file(
                TestKnownGoodRoot::Versions,
                format!("{CRASH_E2E_FABRIC_VERSION_ID}/{CRASH_E2E_FABRIC_VERSION_ID}.jar"),
                KnownGoodArtifactKind::ClientJar,
                &client_jar,
            ),
            file(
                runtime_kind(),
                ".axial-runtime-manifest.json".to_string(),
                KnownGoodArtifactKind::RuntimeManifestProof,
                &runtime_proof,
            ),
            file(
                runtime_kind(),
                ".axial-ready".to_string(),
                KnownGoodArtifactKind::RuntimeReadyMarker,
                &runtime_marker,
            ),
            file(
                runtime_kind(),
                runtime_java_relative,
                KnownGoodArtifactKind::RuntimeExecutable,
                &runtime_java,
            ),
        ];
        entries.extend(library_entries);
        state.activate_known_good_inventory_for_test(
            instance_id,
            KnownGoodInventory::from_test_entries(entries).expect("Fabric crash inventory"),
        );
    }

    fn test_app_state_with_telemetry(root: &Path) -> AppState {
        let paths = test_paths(root);
        let root_session = crate::state::test_root_session(&paths);
        let config_store = ConfigStore::from_config(
            paths.clone(),
            Arc::clone(&root_session),
            AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
                ..AppConfig::default()
            },
        )
        .expect("seed telemetry config");
        let config = Arc::new(config_store);
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("load instances"),
        );
        let telemetry = Arc::new(TelemetryHub::new(
            config.clone(),
            Some(TEST_TELEMETRY_KEY.to_string()),
            DEFAULT_POSTHOG_HOST.to_string(),
        ));

        AppState::new_with_telemetry(
            AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(paths.performance_dir())
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
            },
            telemetry,
        )
    }

    #[cfg(unix)]
    fn test_app_state_with_library(root: &Path) -> AppState {
        let paths = test_paths(root);
        let root_session = crate::state::test_root_session(&paths);
        let config_store = ConfigStore::from_config(
            paths.clone(),
            Arc::clone(&root_session),
            AppConfig {
                library_dir: paths.library_dir().to_string_lossy().to_string(),
                ..AppConfig::default()
            },
        )
        .expect("set test library");
        let config = Arc::new(config_store);
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("load instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }

    fn test_record(session_id: &str) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
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
            guardian: None,
            outcome: None,
            stages: Vec::new(),
        }
    }

    fn test_asset_launch_plan(
        asset_index_id: &str,
        requires_virtual_asset_repair: bool,
    ) -> axial_launcher::VanillaLaunchPlan {
        axial_launcher::VanillaLaunchPlan {
            version: serde_json::from_value(serde_json::json!({
                "id": "test",
                "assetIndex": { "id": asset_index_id }
            }))
            .expect("test version"),
            requires_virtual_asset_repair,
            libraries: Vec::new(),
            client_jar_path: None,
            natives_dir: None,
            classpath: String::new(),
            jvm_args: Vec::new(),
            game_args: Vec::new(),
            main_class: String::new(),
            command: Vec::new(),
            game_dir: PathBuf::new(),
        }
    }

    fn write_runner_asset_index(root: &Path, asset_index_id: &str, contents: &str) {
        let indexes_dir = assets_dir(root).join("indexes");
        fs::create_dir_all(&indexes_dir).expect("asset indexes directory");
        fs::write(indexes_dir.join(format!("{asset_index_id}.json")), contents)
            .expect("asset index");
    }

    #[cfg(unix)]
    fn write_out_of_memory_launch_fixture(root: &Path) -> String {
        let java_path = write_out_of_memory_launch_fixture_with_boot(root, false);
        write_fabric_crash_install(root);
        java_path
    }

    #[cfg(unix)]
    fn write_delayed_boot_launch_fixture(root: &Path) -> String {
        write_crashing_java_fixture(
            root,
            "delayed-boot-java",
            r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
sleep 0.2
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
        )
    }

    #[cfg(unix)]
    async fn serve_registered_library_once(
        body: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind deleted-Libraries source server");
        let address = listener
            .local_addr()
            .expect("deleted-Libraries source address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept deleted-Libraries source request");
            let mut request = [0_u8; 2048];
            let count = socket
                .read(&mut request)
                .await
                .expect("read deleted-Libraries source request");
            assert!(request[..count].starts_with(b"GET "));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write deleted-Libraries source headers");
            socket
                .write_all(body)
                .await
                .expect("write deleted-Libraries source body");
            socket
                .shutdown()
                .await
                .expect("close deleted-Libraries source response");
        });
        (format!("http://{address}/fixture-1.0.0.jar"), server)
    }

    #[cfg(unix)]
    fn write_deleted_library_launch_fixture(
        root: &Path,
        library_path: &Path,
        process_count_path: &Path,
    ) -> String {
        let library_path = shell_path_literal(library_path);
        let process_count_path = shell_path_literal(process_count_path);
        write_crashing_java_fixture(
            root,
            "deleted-library-java",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f {process_count} ]; then
  count=$(cat {process_count})
fi
count=$((count + 1))
printf '%s' "$count" > {process_count}
if [ ! -f {library} ]; then
  mkdir -p crash-reports
  cat > crash-reports/crash-guardian-missing-library.txt <<'CRASH'
---- Minecraft Crash Report ----
Description: Loading game
net.minecraftforge.fml.common.MissingModsException: missing launcher-managed library
CRASH
  printf '%s\n' 'net.minecraftforge.fml.common.MissingModsException: missing launcher-managed library' >&2
  exit 1
fi
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
                process_count = process_count_path,
                library = library_path,
            ),
        )
    }

    #[cfg(unix)]
    fn write_version_bundle_launch_fixture(
        root: &Path,
        client_path: &Path,
        expected_client: &[u8],
        process_count_path: &Path,
    ) -> String {
        let expected_path = root.join("expected-version-bundle-client.jar");
        fs::write(&expected_path, expected_client).expect("expected VersionBundle client fixture");
        let client_path = shell_path_literal(client_path);
        let expected_path = shell_path_literal(&expected_path);
        let process_count_path = shell_path_literal(process_count_path);
        let bin_dir = root.join("wrong-content-client-java").join("bin");
        fs::create_dir_all(&bin_dir).expect("VersionBundle Java bin directory");
        let java_path = bin_dir.join("java");
        fs::write(
            &java_path,
            format!(
                r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f {process_count} ]; then
  count=$(cat {process_count})
fi
count=$((count + 1))
printf '%s' "$count" > {process_count}
if ! cmp -s {client} {expected}; then
  printf '%s\n' 'java.lang.SecurityException: Invalid signature file digest for Manifest main attributes' >&2
  exit 1
fi
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
                process_count = process_count_path,
                client = client_path,
                expected = expected_path,
            ),
        )
        .expect("write VersionBundle Java fixture");
        let mut permissions = fs::metadata(&java_path)
            .expect("VersionBundle Java metadata")
            .permissions();
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("VersionBundle Java executable");
        java_path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    fn shell_path_literal(path: &Path) -> String {
        format!(
            "'{}'",
            path.to_str()
                .expect("test shell fixture path must be UTF-8")
                .replace('\'', "'\"'\"'")
        )
    }

    #[cfg(unix)]
    fn write_post_boot_out_of_memory_launch_fixture(root: &Path) -> String {
        write_out_of_memory_launch_fixture_with_boot(root, true)
    }

    #[cfg(unix)]
    fn write_post_boot_mod_crash_launch_fixture(root: &Path) -> String {
        let java_path = write_crashing_java_fixture(
            root,
            "mod-crash-java",
            r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
mkdir -p crash-reports
cat > crash-reports/crash-guardian-mod.txt <<'CRASH'
---- Minecraft Crash Report ----
Description: Mod loading error has occurred
java.lang.IllegalStateException: registry failed
-- MOD examplemachines --
Failure message: Example Machines (examplemachines) encountered an error
Mod Version: 3.2.1
JVM Flags: -Duser.home=/home/alice -Dtoken=raw-secret-token
CRASH
printf '%s\n' 'java.lang.IllegalStateException: registry failed' >&2
exit 1
"#,
        );
        write_fabric_crash_install(root);
        java_path
    }

    #[cfg(unix)]
    fn write_fabric_crash_install(root: &Path) {
        let libraries = CRASH_E2E_FABRIC_LIBRARIES
            .into_iter()
            .map(|(name, relative_path)| {
                let path = root.join("library").join("libraries").join(relative_path);
                fs::create_dir_all(path.parent().expect("Fabric library parent"))
                    .expect("Fabric library directory");
                write_readable_test_jar(&path);
                let bytes = fs::read(path).expect("read Fabric crash fixture library");
                serde_json::json!({
                    "name": name,
                    "downloads": {
                        "artifact": {
                            "path": relative_path,
                            "sha1": hex::encode(Sha1::digest(&bytes)),
                            "size": bytes.len()
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        let version_dir = root
            .join("library")
            .join("versions")
            .join(CRASH_E2E_FABRIC_VERSION_ID);
        fs::create_dir_all(&version_dir).expect("Fabric crash fixture version directory");
        fs::write(
            version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.json")),
            serde_json::to_vec(&serde_json::json!({
                "id": CRASH_E2E_FABRIC_VERSION_ID,
                "inheritsFrom": "1.21.1",
                "axialMaterialized": true,
                "type": "release",
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "assetIndex": {},
                "arguments": { "jvm": [], "game": [] },
                "libraries": libraries
            }))
            .expect("encode Fabric crash fixture version"),
        )
        .expect("write Fabric crash fixture version");
        fs::copy(
            root.join("library/versions/1.21.1/1.21.1.jar"),
            version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.jar")),
        )
        .expect("materialize Fabric crash fixture client");
    }

    #[cfg(unix)]
    fn write_readable_test_jar(path: &Path) {
        let file = fs::File::create(path).expect("create Fabric crash fixture library");
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "META-INF/guardian-fixture",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start Fabric crash fixture jar entry");
        archive
            .write_all(b"fixture")
            .expect("write Fabric crash fixture jar entry");
        archive
            .finish()
            .expect("finish Fabric crash fixture library");
    }

    #[cfg(unix)]
    fn write_out_of_memory_launch_fixture_with_boot(root: &Path, boot_first: bool) -> String {
        let boot_marker = if boot_first {
            "printf '%s\\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2\nsleep 0.1\n"
        } else {
            ""
        };
        write_crashing_java_fixture(
            root,
            "oom-java",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
{boot_marker}
mkdir -p crash-reports
cat > crash-reports/crash-guardian-oom.txt <<'CRASH'
---- Minecraft Crash Report ----
Description: Rendering game
java.lang.OutOfMemoryError: Java heap space
JVM Flags: -Duser.home=/home/alice -Dtoken=raw-secret-token
Player: SecretPlayer
CRASH
printf '%s\n' 'java.lang.OutOfMemoryError: Java heap space /home/alice/.axial/secret --accessToken raw-secret-token -Xmx8192M -Dtoken=raw provider_payload=provider-secret account_id=account-secret username=SecretPlayer eyJheader123456789.abcdEFGH12345678.ijklMNOP12345678' >&2
printf '%s\n' 'at SecretMod.crash(/home/alice/SecretMod.java:42)' >&2
exit 1
"#
            ),
        )
    }

    #[cfg(unix)]
    fn write_crashing_java_fixture(root: &Path, runtime_dir: &str, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let version_dir = root.join("library").join("versions").join("1.21.1");
        fs::create_dir_all(&version_dir).expect("crash fixture version directory");
        fs::write(
            version_dir.join("1.21.1.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }))
            .expect("encode crash fixture version"),
        )
        .expect("write crash fixture version");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar")
            .expect("write crash fixture client jar");
        fs::create_dir_all(root.join("instance")).expect("crash fixture game directory");

        let bin_dir = root.join(runtime_dir).join("bin");
        fs::create_dir_all(&bin_dir).expect("crash fixture Java bin directory");
        let java_path = bin_dir.join("java");
        fs::write(&java_path, script).expect("write crash fixture Java");
        let mut permissions = fs::metadata(&java_path)
            .expect("crash fixture Java metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("make crash fixture Java executable");
        java_path.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    fn assert_out_of_memory_observation(
        entry: &crate::state::failure_memory::GuardianFailureMemoryEntry,
        expected_instance_id: &str,
    ) {
        assert_eq!(entry.diagnosis_id.as_str(), "out_of_memory");
        assert_eq!(entry.domain, GuardianDomain::Startup);
        assert_eq!(entry.mode, GuardianMode::Managed);
        assert_eq!(entry.target.system, StabilizationSystem::Guardian);
        assert_eq!(entry.target.kind, TargetKind::Instance);
        assert_eq!(entry.target.id, expected_instance_id);
        assert_eq!(entry.ownership, OwnershipClass::UserOwned);
        assert_eq!(entry.occurrence_count, 1);
        assert_eq!(entry.last_action_kind, None);
        assert_eq!(entry.last_action_outcome, None);
        assert_eq!(entry.repair_attempt_count, 0);
        assert_eq!(entry.suppression_until, None);
        assert_eq!(entry.target_content_hash, None);
        assert_eq!(entry.user_intent_hash, None);
    }

    #[cfg(unix)]
    fn assert_oom_preflight_guidance(
        preflight: &super::super::session::LaunchPreflightResponse,
        fact: &crate::guardian::GuardianFact,
    ) {
        let current_memory_mb = fact
            .fields
            .iter()
            .find(|field| field.key == "current_memory_mb")
            .map(|field| field.value.as_str())
            .expect("OOM fact current memory");
        let suggested_memory_mb = fact
            .fields
            .iter()
            .find(|field| field.key == "suggested_memory_mb")
            .map(|field| field.value.as_str());
        let expected = suggested_memory_mb.map_or_else(
            || {
                "Guardian could not verify safe headroom for a larger memory allocation. Close another session or free memory before relaunching."
                    .to_string()
            },
            |suggested| {
                format!(
                    "Increase this instance's maximum memory from {current_memory_mb} MB to {suggested} MB before relaunching."
                )
            },
        );
        assert!(
            preflight
                .guardian
                .guidance()
                .iter()
                .any(|guidance| guidance == &expected),
            "missing OOM next-launch guidance: {expected}"
        );
    }

    #[cfg(unix)]
    fn guardian_fact_field<'a>(
        fact: &'a crate::guardian::GuardianFact,
        key: &str,
    ) -> Option<&'a str> {
        fact.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.value.as_str())
    }

    #[cfg(unix)]
    fn assert_no_out_of_memory_sensitive_decoys(payload: &str) {
        for fragment in [
            "/home/alice",
            "C:\\Users\\Alice",
            "--accessToken",
            "raw-secret-token",
            "-Xmx8192M",
            "-Dtoken=raw",
            "provider_payload",
            "provider-secret",
            "account_id=account-secret",
            "username=SecretPlayer",
            "SecretPlayer",
            "SecretMod.crash",
            "eyJheader123456789",
        ] {
            assert!(
                !payload.contains(fragment),
                "OOM public or persisted payload leaked {fragment:?}: {payload}"
            );
        }
    }

    fn assert_no_sensitive_stage_evidence(text: &str) {
        for fragment in [
            "/home/alice",
            "/home/",
            "C:\\Users",
            "Alice",
            ".jdks",
            ".minecraft",
            "java.exe",
            "--accessToken",
            "-Xmx",
            "-cp",
            "token",
            "SecretPlayer",
        ] {
            assert!(
                !text.contains(fragment),
                "stage evidence leaked fragment {fragment:?}: {text}"
            );
        }
    }

    fn write_managed_runtime_launch_fixture(root: &Path) -> LaunchIntent {
        let library_dir = root.join("library");
        let version_dir = library_dir.join("versions").join("1.21.1");
        let game_dir = root.join("instance");
        fs::create_dir_all(&version_dir).expect("managed launch version directory");
        fs::create_dir_all(&game_dir).expect("managed launch game directory");
        fs::create_dir_all(library_dir.join("assets/indexes"))
            .expect("managed launch asset indexes");
        fs::write(
            version_dir.join("1.21.1.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": { "id": "launch-runtime-epoch" },
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "arguments": { "jvm": [], "game": [] },
                "libraries": []
            }))
            .expect("managed launch version JSON"),
        )
        .expect("managed launch version metadata");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar")
            .expect("managed launch client jar");
        fs::write(
            library_dir.join("assets/indexes/launch-runtime-epoch.json"),
            br#"{"objects":{}}"#,
        )
        .expect("managed launch asset index");

        LaunchIntent {
            library_dir,
            version_id: "1.21.1".to_string(),
            target_version_id: "1.21.1".to_string(),
            loader: "vanilla".to_string(),
            is_modded: false,
            auth: LaunchAuthContext::offline("Player"),
            requested_java: String::new(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 4096,
            min_memory_mb: 1024,
            resolution: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            game_dir: Some(game_dir),
            guardian: LaunchGuardianContext {
                mode: axial_launcher::GuardianMode::Managed,
                java_override_origin: None,
                preset_override_origin: None,
                raw_jvm_args_origin: None,
            },
            low_impact_startup: true,
        }
    }

    fn managed_runtime_java_path_for_runner_test(runtime_root: &Path) -> PathBuf {
        if cfg!(target_os = "macos") {
            return runtime_root.join("jre.bundle/Contents/Home/bin/java");
        }
        runtime_root
            .join("bin")
            .join(if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            })
    }

    async fn serve_launch_runtime_once(
        bytes: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("launch Runtime fixture server");
        let address = listener
            .local_addr()
            .expect("launch Runtime fixture address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("launch Runtime fixture request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("launch Runtime response headers");
            socket
                .write_all(bytes)
                .await
                .expect("launch Runtime response body");
        });
        (format!("http://{address}/java"), server)
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
