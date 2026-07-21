//! Application-owned launch workflow orchestration and presentation.
use crate::guardian::{
    GuardianPreflightOutcome, GuardianSummaryDecision, guardian_launch_stage_evidence,
};
use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_evidence_token};
use axial_launcher::LaunchStageEvidence;
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

mod benchmark;
mod policy;
mod reports;
mod runner;
mod session;

pub(crate) use crate::guardian::launch_notice;

pub(crate) use super::performance::BenchmarkMatrix;
pub(crate) use benchmark::*;
#[cfg(test)]
pub(crate) use reports::{
    LAUNCH_COMMAND_REDACTED_VALUE, LAUNCH_KILL_INTERNAL_ERROR_MESSAGE,
    LAUNCH_KILL_NO_PROCESS_MESSAGE, launch_kill_error_response, sanitize_launch_command,
};
pub use reports::{LaunchStatusViewModel, PublicLaunchStatus, public_launch_status};
pub(crate) use reports::{
    launch_command_payload, launch_report_payload, launch_reports_payload, launch_status,
    stop_launch_session,
};
pub(crate) use runner::LaunchRequestError;
pub(crate) use runner::launch_session;
#[cfg(all(test, unix))]
pub(crate) use runner::launch_session_with_persisted_runtime_manifest_for_test;
use runner::{LaunchSuccess, sanitize_live_launch_failure_message, trace_launch_event};

#[cfg(all(test, unix))]
use session::prepare_launch_session;
#[cfg(test)]
pub(crate) use session::readiness_guardian_facts_for_coverage;
pub use session::{
    LaunchPreflightMemory, LaunchPreflightOverride, LaunchPreflightOverrides,
    LaunchPreflightResourceBudget, LaunchPreflightResponse, LaunchRequest,
    prepare_launch_preflight,
};
pub(crate) use session::{LaunchSessionTask, prepare_launch_session_owned};

pub type LaunchApplicationError = (StatusCode, Json<Value>);

pub(crate) fn launch_shutdown_error_response(
    _error: crate::state::LifecycleAdmissionError,
) -> LaunchApplicationError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "application shutdown is in progress" })),
    )
}

pub(crate) fn launch_application_stage_evidence() -> Vec<LaunchStageEvidence> {
    vec![launch_stage_evidence(
        "application_launch_command_staged",
        "application",
        "Application staged the launch command.",
        vec![
            "command:launch_instance".to_string(),
            "session_state:queued".to_string(),
            "status:Planned".to_string(),
        ],
    )]
}

pub(crate) fn launch_preflight_stage_evidence(
    outcome: &GuardianPreflightOutcome,
    performance_mode: &str,
) -> Vec<LaunchStageEvidence> {
    let performance_mode =
        sanitize_evidence_token(performance_mode, RedactionAudience::UserVisible, 64)
            .unwrap_or_else(|| "unknown".to_string());
    vec![
        guardian_launch_stage_evidence(outcome),
        launch_stage_evidence(
            "performance_launch_plan_input",
            "performance",
            "Performance launch inputs were recorded.",
            vec![format!("mode:{performance_mode}")],
        ),
    ]
}

pub(crate) fn launch_benchmark_status_payload(
    benchmark: &crate::state::launch_reports::LaunchBenchmarkMetadata,
) -> Value {
    let mut payload = json!({
        "id": benchmark.benchmark_id,
        "profile": benchmark.profile,
        "run_type": benchmark.run_type,
    });
    if let Some(mode) = &benchmark.mode {
        payload["mode"] = json!(mode);
    }
    payload
}

pub(crate) fn launch_prepared_response_payload(
    task: &LaunchSessionTask,
    status: &PublicLaunchStatus,
) -> Value {
    let mut response = json!(status);
    response["instance_id"] = json!(&task.intent.instance_id);
    response["launched_at"] = json!(&task.launched_at);
    response["max_memory_mb"] = json!(task.intent.max_memory_mb);
    response["min_memory_mb"] = json!(task.intent.min_memory_mb);
    response
}

fn launch_request_error_response_payload(error: &LaunchRequestError) -> Value {
    let public_message = sanitize_live_launch_failure_message(&error.message);
    let notice = launch_notice(
        error.guardian.as_ref(),
        error.healing.as_ref(),
        None,
        Some(public_message.as_str()),
        Some("Launch stopped before startup."),
    );
    json!({
        "error": public_message,
        "healing": error.healing,
        "guardian": error.guardian,
        "notice": notice,
    })
}

pub(crate) fn launch_request_error_response(error: LaunchRequestError) -> LaunchApplicationError {
    let status = launch_request_error_status(&error);
    (status, Json(launch_request_error_response_payload(&error)))
}

pub(crate) fn launch_request_error_status(error: &LaunchRequestError) -> StatusCode {
    if error
        .guardian
        .as_ref()
        .is_some_and(|guardian| guardian.decision() == GuardianSummaryDecision::Blocked)
    {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

fn launch_stage_evidence(
    id: &str,
    system: &str,
    summary: &str,
    details: Vec<String>,
) -> LaunchStageEvidence {
    LaunchStageEvidence {
        id: sanitize_evidence_token(id, RedactionAudience::UserVisible, 64)
            .unwrap_or_else(|| "launch_stage_evidence".to_string()),
        system: sanitize_evidence_token(system, RedactionAudience::UserVisible, 32)
            .unwrap_or_else(|| "application".to_string()),
        summary: sanitize_evidence_text(summary, RedactionAudience::UserVisible, 160)
            .unwrap_or_else(|| "Launch stage evidence recorded.".to_string()),
        details: details
            .into_iter()
            .filter_map(|detail| {
                sanitize_evidence_text(&detail, RedactionAudience::UserVisible, 120)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::launch_application_stage_evidence;
    use crate::execution::ExecutionFactKind;
    use crate::execution::runtime::runtime_fact;
    use crate::guardian::guardian_fact_from_execution;
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use axial_launcher::LaunchStageEvidence;

    #[test]
    fn p00_b07_contract_launch_stage_evidence_is_unchanged() {
        let evidence = launch_application_stage_evidence();

        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].id, "application_launch_command_staged");
        assert_eq!(evidence[0].system, "application");
        assert_eq!(
            evidence[0].summary,
            "Application staged the launch command."
        );
        assert_eq!(
            evidence[0].details,
            [
                "command:launch_instance",
                "session_state:queued",
                "status:Planned",
            ]
        );
    }

    #[test]
    fn launch_preflight_stage_evidence_has_exact_shape_and_redacts_performance_mode() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            "manual_java",
            OwnershipClass::UserOwned,
        );
        let execution_fact = runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            None,
            &target,
            Vec::new(),
        );
        let guardian_fact =
            guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);

        let outcome = crate::guardian::guardian_preflight_outcome(
            crate::guardian::GuardianPreflightOutcomeRequest::new(
                crate::guardian::GuardianMode::Managed,
                &[guardian_fact],
            ),
        );
        let evidence =
            super::launch_preflight_stage_evidence(&outcome, r"managed C:\Users\Alice -Xmx8192M");
        assert_eq!(
            evidence,
            vec![
                LaunchStageEvidence {
                    id: "guardian_launch_safety_decision".to_string(),
                    system: "guardian".to_string(),
                    summary: "Guardian recorded the launch safety decision.".to_string(),
                    details: vec![
                        "mode:Managed".to_string(),
                        "decision:Fallback".to_string(),
                        "diagnoses:1".to_string(),
                    ],
                },
                LaunchStageEvidence {
                    id: "performance_launch_plan_input".to_string(),
                    system: "performance".to_string(),
                    summary: "Performance launch inputs were recorded.".to_string(),
                    details: vec!["mode:unknown".to_string()],
                },
            ]
        );
        let encoded = serde_json::to_string(&evidence).expect("stage evidence json");

        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("-Xmx"));
        assert!(!encoded.contains("Users"));
    }

    #[test]
    fn launch_preflight_stage_evidence_records_effective_warning_verdict() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Instance,
            "instance-a",
            OwnershipClass::UserOwned,
        );
        let historical_fact = crate::guardian::GuardianFact {
            operation_id: None,
            id: crate::guardian::GuardianFactId::RecentStartupFailure,
            domain: crate::guardian::GuardianDomain::Startup,
            phase: OperationPhase::Validating,
            reliability: crate::guardian::FactReliability::DirectStructured,
            severity: Some(crate::guardian::GuardianSeverity::Warning),
            confidence: Some(crate::guardian::GuardianConfidence::Confirmed),
            ownership: target.ownership,
            target: Some(target),
            fields: Vec::new(),
        };
        let outcome = crate::guardian::guardian_preflight_outcome(
            crate::guardian::GuardianPreflightOutcomeRequest::new(
                crate::guardian::GuardianMode::Managed,
                &[historical_fact],
            ),
        );

        assert_eq!(
            outcome.guardian_decision.kind(),
            crate::guardian::GuardianActionKind::Warn
        );
        assert_eq!(
            outcome.user_outcome.decision(),
            crate::guardian::GuardianActionKind::Warn
        );
        assert_eq!(
            super::launch_preflight_stage_evidence(&outcome, "managed")[0].details,
            vec![
                "mode:Managed".to_string(),
                "decision:Warn".to_string(),
                "diagnoses:1".to_string(),
            ]
        );
    }
}
