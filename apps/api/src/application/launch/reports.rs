use super::runner::persist_launch_proof_owned;
use crate::guardian::{GuardianSummary, guardian_proof_evidence, launch_notice_from_values};
use crate::observability::{
    RedactionAudience, sanitize_evidence_token, sanitize_public_diagnostic_text,
    sanitize_public_json_value,
};
use crate::state::{AppState, LaunchStatusEvent, RevisionedLaunchStatus, SessionStopError};
use axial_launcher::{
    CrashEvidence, LaunchHealingSummary, LaunchNotice, LaunchSessionOutcome, LaunchSessionRecord,
    LaunchStageEvidence, LaunchStageRecord,
};
use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) const LAUNCH_COMMAND_REDACTED_VALUE: &str = "<redacted>";
pub(crate) const LAUNCH_KILL_INTERNAL_ERROR_MESSAGE: &str =
    "Could not stop the launch session. Try again from the launcher.";
pub(crate) const LAUNCH_KILL_NO_PROCESS_MESSAGE: &str = "session has no running process";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LaunchStatusViewModel {
    pub state_id: String,
    pub label: String,
    pub progress_pct: u8,
    pub terminal: bool,
    pub playing: bool,
    pub process_live: bool,
    pub can_stop: bool,
}

impl LaunchStatusViewModel {
    fn for_state(state: &str) -> Self {
        let (state_id, label, progress_pct, terminal) = launch_status_view_fields(state);
        Self {
            state_id: state_id.to_string(),
            label: label.to_string(),
            progress_pct,
            terminal,
            playing: matches!(state, "running" | "degraded"),
            process_live: false,
            can_stop: false,
        }
    }

    fn for_status(status: &LaunchStatusEvent) -> Self {
        let mut view_model = Self::for_state(&status.state);
        view_model.process_live = status.pid.is_some()
            && !matches!(
                status.state.as_str(),
                "recovering" | "settling" | "failed" | "exited"
            );
        view_model.can_stop = view_model.process_live;
        view_model
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PublicLaunchStatus {
    pub session_id: String,
    pub revision: u64,
    pub state: String,
    pub benchmark: Option<Value>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub failure_class: Option<String>,
    pub failure_detail: Option<String>,
    pub crash_evidence: Option<CrashEvidence>,
    pub healing: Option<Value>,
    pub guardian: Option<Value>,
    pub outcome: Option<LaunchSessionOutcome>,
    pub notice: Option<LaunchNotice>,
    pub evidence: Vec<LaunchStageEvidence>,
    pub stages: Vec<LaunchStageRecord>,
    pub view_model: LaunchStatusViewModel,
}

fn launch_status_view_fields(state: &str) -> (&'static str, &'static str, u8, bool) {
    match state {
        "queued" => ("queued", "Preparing launch", 8, false),
        "planning" => ("planning", "Planning launch", 18, false),
        "validating" => ("validating", "Validating launch", 24, false),
        "ensuring_runtime" => ("ensuring_runtime", "Ensuring runtime", 34, false),
        "downloading_runtime" => ("downloading_runtime", "Downloading runtime", 42, false),
        "preparing" => ("preparing", "Preparing files", 56, false),
        "starting" => ("starting", "Starting process", 72, false),
        "monitoring" => ("monitoring", "Monitoring startup", 88, false),
        "recovering" => ("recovering", "Recovering startup", 88, false),
        "running" => ("running", "Running", 100, false),
        "degraded" => ("degraded", "Running with warnings", 100, false),
        "settling" => ("settling", "Finalizing session", 100, false),
        "failed" => ("failed", "Launch failed", 100, true),
        "exited" => ("exited", "Exited", 100, true),
        _ => ("unknown", "Launch status updated", 0, false),
    }
}

pub(crate) fn launch_reports_payload(state: &AppState) -> serde_json::Value {
    let reports = state.launch_reports().list_recent_exports(25);

    json!({
        "reports": reports
            .iter()
            .map(launch_proof_export_payload)
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn launch_report_payload(
    state: &AppState,
    id: &str,
) -> Result<serde_json::Value, super::LaunchApplicationError> {
    let report = state.launch_reports().load_export(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "launch report not found" })),
        )
    })?;

    Ok(launch_proof_export_payload(&report))
}

fn launch_proof_export_payload(report: &crate::state::launch_reports::LaunchProofExport) -> Value {
    let mut payload = serde_json::to_value(report).unwrap_or_else(|_| json!({}));
    payload["view_model"] = launch_proof_view_model(report);
    payload
}

fn launch_proof_view_model(report: &crate::state::launch_reports::LaunchProofExport) -> Value {
    json!({
        "outcome_label": public_token_label(&report.outcome, "Unknown"),
        "outcome_tone": launch_proof_outcome_tone(&report.outcome),
        "evidence": launch_proof_evidence_view_model(report.guardian.as_ref(), report.healing.as_ref()),
        "comparison": launch_proof_comparison_view_model(report.comparison.as_ref()),
        "resource_budget": launch_proof_resource_budget_view_model(report.resource_budget.as_ref()),
    })
}

fn launch_proof_outcome_tone(outcome: &str) -> &'static str {
    match outcome.trim().to_ascii_lowercase().as_str() {
        "running" | "completed" | "exited" => "ok",
        "stopped" | "cancelled" | "canceled" => "warn",
        value if value.contains("fail") || value.contains("crash") || value == "error" => "err",
        _ => "neutral",
    }
}

fn launch_proof_evidence_view_model(
    guardian: Option<&GuardianSummary>,
    healing: Option<&LaunchHealingSummary>,
) -> Option<Value> {
    guardian
        .and_then(guardian_proof_evidence_view_model)
        .or_else(|| healing.and_then(healing_proof_evidence_view_model))
}

fn guardian_proof_evidence_view_model(guardian: &GuardianSummary) -> Option<Value> {
    guardian_proof_evidence(guardian).map(|evidence| json!(evidence))
}

fn healing_proof_evidence_view_model(healing: &LaunchHealingSummary) -> Option<Value> {
    let retry_count = healing.retry_count.unwrap_or(0);
    let has_evidence = retry_count > 0
        || healing.fallback_applied.is_some()
        || healing.failure_class.is_some()
        || !healing.warnings.is_empty();
    if !has_evidence {
        return None;
    }

    let detail = first_bounded_public_detail(
        healing
            .fallback_applied
            .iter()
            .cloned()
            .chain(healing.warnings.iter().cloned())
            .chain(
                healing
                    .events
                    .iter()
                    .filter_map(|event| event.detail.clone()),
            )
            .chain(healing.failure_class.iter().map(|failure_class| {
                format!(
                    "Reason: {}",
                    public_token_label(failure_class, "launch failure")
                )
            })),
    );
    let label = if retry_count > 0 {
        format!(
            "Healing retried {retry_count} {}",
            if retry_count == 1 { "time" } else { "times" }
        )
    } else if healing.failure_class.is_some() {
        "Healing failure".to_string()
    } else {
        "Healing applied".to_string()
    };

    Some(json!({
        "tone": if healing.failure_class.is_some() {
            "err"
        } else if retry_count > 0 {
            "ok"
        } else {
            "info"
        },
        "label": label,
        "detail": detail,
    }))
}

fn launch_proof_comparison_view_model(
    comparison: Option<&crate::state::launch_reports::LaunchProofComparison>,
) -> Value {
    let Some(comparison) = comparison else {
        return json!({
            "label": "No baseline",
            "detail": "No comparable local proof yet",
            "tone": "neutral",
        });
    };

    let percent = launch_proof_percent_label(comparison.delta_percent);
    let current = launch_proof_duration_label(comparison.current_value_ms);
    let baseline = launch_proof_duration_label(comparison.baseline_value_ms);
    let proof_label = if comparison.matched_sample_count == 1 {
        "proof"
    } else {
        "proofs"
    };
    let detail = format!(
        "{current} now, {baseline} baseline, {} matched {proof_label}",
        comparison.matched_sample_count
    );
    let (faster_by, slower_by, matches_baseline) = comparison_metric_copy(&comparison.metric_name);

    if comparison.delta_ms < 0 {
        json!({
            "label": format!("{} {} ({}%)", faster_by, launch_proof_signed_duration_label(comparison.delta_ms), percent),
            "detail": detail,
            "tone": "ok",
        })
    } else if comparison.delta_ms > 0 {
        json!({
            "label": format!("{} {} ({}%)", slower_by, launch_proof_signed_duration_label(comparison.delta_ms), percent),
            "detail": detail,
            "tone": "warn",
        })
    } else {
        json!({
            "label": matches_baseline,
            "detail": detail,
            "tone": "neutral",
        })
    }
}

fn launch_proof_resource_budget_view_model(
    resource_budget: Option<&crate::state::launch_reports::LaunchProofResourceBudget>,
) -> Option<Value> {
    let resource_budget = resource_budget?;
    let mut pressures = Vec::new();
    if resource_budget.memory_pressure {
        pressures.push("memory");
    }
    if resource_budget.cpu_pressure {
        pressures.push("CPU");
    }
    if resource_budget.install_pressure {
        pressures.push("installs");
    }
    if resource_budget.disk_pressure {
        pressures.push("disk");
    }

    let mut details = Vec::new();
    if let Some(value) = resource_budget.estimated_remaining_memory_mb {
        details.push(format!(
            "{} remaining",
            launch_proof_signed_memory_label(value)
        ));
    } else if let Some(value) = resource_budget.host_available_memory_mb {
        details.push(format!("{} available", launch_proof_memory_label(value)));
    } else if let Some(value) = resource_budget.host_used_memory_mb {
        details.push(format!("{} used", launch_proof_memory_label(value)));
    } else if let Some(value) = resource_budget.launcher_process_memory_mb {
        details.push(format!("{} launcher RSS", launch_proof_memory_label(value)));
    }

    if let Some(value) = resource_budget.host_cpu_load_1m_x100 {
        let threads = resource_budget
            .host_cpu_threads
            .filter(|threads| *threads > 0)
            .map(|threads| format!("/{threads} threads"))
            .unwrap_or_default();
        details.push(format!(
            "load {}{}",
            launch_proof_load_average_label(value),
            threads
        ));
    }

    if resource_budget.active_session_count > 0 {
        let allocation = if resource_budget.active_memory_allocation_mb > 0 {
            format!(
                ", {} allocated",
                launch_proof_memory_label(resource_budget.active_memory_allocation_mb)
            )
        } else {
            String::new()
        };
        details.push(format!(
            "{} active {}{}",
            resource_budget.active_session_count,
            if resource_budget.active_session_count == 1 {
                "session"
            } else {
                "sessions"
            },
            allocation
        ));
    }

    if resource_budget.active_install_count > 0 {
        details.push(format!(
            "{} active {}",
            resource_budget.active_install_count,
            if resource_budget.active_install_count == 1 {
                "install"
            } else {
                "installs"
            }
        ));
    }

    if let Some(value) = resource_budget.launch_disk_available_mb {
        details.push(format!("{} disk free", launch_proof_memory_label(value)));
    }

    Some(json!({
        "pressure_label": if pressures.is_empty() {
            "Pressure clear".to_string()
        } else {
            format!("Pressure: {}", pressures.join(", "))
        },
        "details": details,
        "pressure": !pressures.is_empty(),
    }))
}

fn comparison_metric_copy(metric_name: &str) -> (&'static str, &'static str, &'static str) {
    match metric_name {
        "boot_duration_ms" => ("Boot faster by", "Boot slower by", "Boot matches baseline"),
        "total_completed_stage_duration_ms" => (
            "Launch stages faster by",
            "Launch stages slower by",
            "Launch stages match baseline",
        ),
        _ => ("Faster by", "Slower by", "Matches baseline"),
    }
}

fn first_bounded_public_detail(values: impl IntoIterator<Item = String>) -> Option<String> {
    values.into_iter().find_map(|value| {
        let detail =
            sanitize_public_diagnostic_text(&value, RedactionAudience::UserVisible, 150, "");
        if detail.is_empty() {
            None
        } else {
            Some(detail)
        }
    })
}

fn launch_proof_duration_label(value_ms: u64) -> String {
    if value_ms >= 1000 {
        if value_ms >= 10_000 {
            format!("{}s", (value_ms + 500) / 1000)
        } else {
            let tenths = (value_ms + 50) / 100;
            format!("{}.{:01}s", tenths / 10, tenths % 10)
        }
    } else {
        format!("{value_ms}ms")
    }
}

fn launch_proof_memory_label(value_mb: u64) -> String {
    if value_mb >= 1024 {
        let whole = value_mb / 1024;
        let remainder = value_mb % 1024;
        if remainder == 0 {
            format!("{whole} GB")
        } else {
            let tenths = ((remainder * 10) + 512) / 1024;
            if tenths >= 10 {
                format!("{} GB", whole + 1)
            } else {
                format!("{whole}.{tenths} GB")
            }
        }
    } else {
        format!("{value_mb} MB")
    }
}

fn launch_proof_signed_memory_label(value_mb: i64) -> String {
    if value_mb < 0 {
        format!("-{}", launch_proof_memory_label(value_mb.unsigned_abs()))
    } else {
        launch_proof_memory_label(value_mb as u64)
    }
}

fn launch_proof_load_average_label(value_x100: u64) -> String {
    format!("{}.{:02}", value_x100 / 100, value_x100 % 100)
}

fn launch_proof_signed_duration_label(value_ms: i64) -> String {
    launch_proof_duration_label(value_ms.unsigned_abs())
}

fn launch_proof_percent_label(value: f64) -> String {
    let mut percent = format!("{:.1}", value.abs());
    if percent.ends_with(".0") {
        percent.truncate(percent.len() - 2);
    }
    percent
}

fn public_token_label(value: &str, fallback: &str) -> String {
    let labels = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    if labels.is_empty() {
        fallback.to_string()
    } else {
        labels.join(" ")
    }
}

pub(crate) async fn launch_command_payload(
    state: &AppState,
    id: &str,
) -> Result<serde_json::Value, super::LaunchApplicationError> {
    let record = state.sessions().get(id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    Ok(launch_command_response_payload(&record))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SanitizedLaunchCommand {
    pub(crate) command: Vec<String>,
    pub(crate) redacted: bool,
}

pub(crate) fn launch_command_response_payload(record: &LaunchSessionRecord) -> serde_json::Value {
    // This diagnostic route preserves command shape without exposing the command line.
    let command = sanitize_launch_command(&record.command);

    public_payload(json!({
        "command": command.command,
        "command_redacted": command.redacted,
        "command_arg_count": record.command.len(),
        "java_path_present": record.java_path.is_some(),
        "session_id": record.session_id.0,
        "healing": record.healing,
        "guardian": record.guardian,
    }))
}

pub(crate) fn sanitize_launch_command(command: &[String]) -> SanitizedLaunchCommand {
    SanitizedLaunchCommand {
        command: command
            .iter()
            .map(|_| LAUNCH_COMMAND_REDACTED_VALUE.to_string())
            .collect(),
        redacted: !command.is_empty(),
    }
}

pub(crate) async fn launch_status(
    state: &AppState,
    id: &str,
) -> Result<PublicLaunchStatus, super::LaunchApplicationError> {
    let status = state.sessions().status_snapshot(id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    Ok(public_launch_status(&status))
}

pub fn public_launch_status(status: &RevisionedLaunchStatus) -> PublicLaunchStatus {
    let event = &status.status;
    let view_model = LaunchStatusViewModel::for_status(event);
    debug_assert_eq!(view_model.terminal, event.outcome.is_some());
    let outcome = sanitize_public_typed(event.outcome.as_ref());
    let notice = event.notice.clone().or_else(|| {
        (!matches!(event.state.as_str(), "recovering" | "settling"))
            .then(|| {
                launch_notice_from_values(
                    event.guardian.as_ref(),
                    event.healing.as_ref(),
                    outcome.as_ref(),
                    event.failure_detail.as_deref(),
                    None,
                )
            })
            .flatten()
    });
    let projected = PublicLaunchStatus {
        session_id: status.session_id.clone(),
        revision: status.revision,
        state: event.state.clone(),
        benchmark: sanitize_public_value(event.benchmark.as_ref()),
        pid: event.pid,
        exit_code: event.exit_code,
        failure_class: event
            .failure_class
            .as_deref()
            .and_then(|value| sanitize_evidence_token(value, RedactionAudience::UserVisible, 64)),
        failure_detail: event.failure_detail.as_deref().and_then(|value| {
            let value =
                sanitize_public_diagnostic_text(value, RedactionAudience::UserVisible, 240, "");
            (!value.is_empty()).then_some(value)
        }),
        crash_evidence: sanitize_public_typed(event.crash_evidence.as_ref()),
        healing: sanitize_public_value(event.healing.as_ref()),
        guardian: sanitize_public_value(event.guardian.as_ref()),
        outcome,
        notice: sanitize_public_typed(notice.as_ref()),
        evidence: sanitize_public_items(&event.evidence),
        stages: sanitize_public_items(&event.stages),
        view_model,
    };
    debug_assert_eq!(projected.view_model.terminal, projected.outcome.is_some());
    projected
}

fn sanitize_public_value(value: Option<&Value>) -> Option<Value> {
    value.cloned().and_then(|value| {
        sanitize_public_json_value(value, RedactionAudience::UserVisible, 240, 64)
    })
}

fn sanitize_public_typed<T>(value: Option<&T>) -> Option<T>
where
    T: Serialize + DeserializeOwned,
{
    value
        .and_then(|value| serde_json::to_value(value).ok())
        .and_then(|value| {
            sanitize_public_json_value(value, RedactionAudience::UserVisible, 240, 64)
        })
        .and_then(|value| serde_json::from_value(value).ok())
}

fn sanitize_public_items<T>(values: &[T]) -> Vec<T>
where
    T: Serialize + DeserializeOwned,
{
    values
        .iter()
        .filter_map(|value| sanitize_public_typed(Some(value)))
        .collect()
}

fn public_payload(value: serde_json::Value) -> serde_json::Value {
    sanitize_public_json_value(value, RedactionAudience::UserVisible, 240, 64)
        .unwrap_or_else(|| json!({}))
}

pub(crate) async fn stop_launch_session(
    state: &AppState,
    id: &str,
    producer: &crate::state::ProducerLease,
) -> Result<serde_json::Value, super::LaunchApplicationError> {
    let stop = state
        .sessions()
        .begin_user_stop(id)
        .await
        .map_err(launch_kill_error_response)?;
    let record = stop.record().clone();

    persist_launch_proof_owned(
        state,
        producer,
        id,
        record.launched_at.as_deref(),
        "stopped",
    )
    .await;
    stop.release().await;

    Ok(json!({ "status": "killed" }))
}

pub(crate) fn launch_kill_error_response(error: SessionStopError) -> super::LaunchApplicationError {
    match error {
        SessionStopError::SessionNotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        ),
        SessionStopError::NoLiveProcess => (
            StatusCode::CONFLICT,
            Json(json!({ "error": LAUNCH_KILL_NO_PROCESS_MESSAGE })),
        ),
        SessionStopError::Process(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": LAUNCH_KILL_INTERNAL_ERROR_MESSAGE })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LAUNCH_KILL_NO_PROCESS_MESSAGE, LaunchStatusViewModel, launch_status, public_launch_status,
        stop_launch_session,
    };
    use crate::state::{
        AppState, AppStateInit, InstallStore, LaunchStatusEvent, RevisionedLaunchStatus,
        SessionStore,
    };
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    #[cfg(unix)]
    use axial_launcher::LaunchEvent;
    use axial_launcher::{
        CrashEvidence, LaunchNotice, LaunchNoticeTone, LaunchSessionExitReason,
        LaunchSessionOutcome, LaunchSessionRecord, LaunchStageEvidence, LaunchStageRecord,
        LaunchState, SessionId,
    };
    use axial_performance::PerformanceManager;
    use axum::http::StatusCode;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(unix)]
    use tokio::process::Command;

    #[tokio::test]
    async fn stop_launch_session_reports_missing_session() {
        let root = unique_test_dir("stop-launch-missing-session");
        let state = test_app_state(&root);
        let producer = state.try_claim_producer().expect("claim stop producer");

        let error = stop_launch_session(&state, "missing-session", &producer)
            .await
            .expect_err("missing session should fail stop");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "session not found" })
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stop_launch_session_reports_existing_session_without_live_process() {
        let root = unique_test_dir("stop-launch-retention-error");
        let state = test_app_state(&root);
        let session_id = "stop-kill-error";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        state
            .sessions()
            .release_terminal_retention_hold(session_id)
            .await;

        let producer = state.try_claim_producer().expect("claim stop producer");
        let error = stop_launch_session(&state, session_id, &producer)
            .await
            .expect_err("missing child should fail stop");
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": LAUNCH_KILL_NO_PROCESS_MESSAGE })
        );

        state
            .sessions()
            .emit_status(session_id, terminal_status())
            .await;
        for index in 0..=32 {
            let completed_id = format!("completed-{index}");
            state
                .sessions()
                .insert(test_record(&completed_id))
                .await
                .expect("insert session");
            state
                .sessions()
                .release_terminal_retention_hold(&completed_id)
                .await;
            state
                .sessions()
                .emit_status(&completed_id, terminal_status())
                .await;
        }

        assert!(state.sessions().get(session_id).await.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_launch_session_persists_the_single_supervisor_terminal_record() {
        let root = unique_test_dir("stop-launch-canonical-terminal");
        let state = test_app_state(&root);
        let session_id = "stop-canonical-terminal";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert stop session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe stop session");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        state
            .sessions()
            .start_process(test_record(session_id), command)
            .await
            .expect("start stop target");
        let producer = state.try_claim_producer().expect("claim stop producer");

        let response = stop_launch_session(&state, session_id, &producer)
            .await
            .expect("stop running session");
        assert_eq!(response, serde_json::json!({ "status": "killed" }));

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("canonical stopped record");
        assert_eq!(record.state, LaunchState::Exited);
        assert_eq!(
            record.outcome.as_ref().expect("stop outcome").reason,
            LaunchSessionExitReason::LauncherStopped
        );
        assert_eq!(
            record
                .stages
                .iter()
                .flat_map(|stage| &stage.evidence)
                .filter(|evidence| evidence.id.contains("process_stop_requested"))
                .count(),
            1
        );
        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("stopped launch proof");
        assert_eq!(proof.outcome, "stopped");
        assert_eq!(proof.session_outcome, record.outcome);

        let mut terminal_count = 0;
        while let Ok(event) = events.try_recv() {
            if let LaunchEvent::Status(status) = event
                && matches!(status.state.as_str(), "failed" | "exited")
            {
                terminal_count += 1;
            }
        }
        assert_eq!(terminal_count, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_status_view_model_authors_progress_copy() {
        assert_eq!(
            LaunchStatusViewModel::for_state("validating"),
            LaunchStatusViewModel {
                state_id: "validating".to_string(),
                label: "Validating launch".to_string(),
                progress_pct: 24,
                terminal: false,
                playing: false,
                process_live: false,
                can_stop: false,
            }
        );
        assert_eq!(
            LaunchStatusViewModel::for_state("exited"),
            LaunchStatusViewModel {
                state_id: "exited".to_string(),
                label: "Exited".to_string(),
                progress_pct: 100,
                terminal: true,
                playing: false,
                process_live: false,
                can_stop: false,
            }
        );
        assert_eq!(
            LaunchStatusViewModel::for_status(&LaunchStatusEvent {
                state: "settling".to_string(),
                pid: Some(42),
                ..terminal_status()
            }),
            LaunchStatusViewModel {
                state_id: "settling".to_string(),
                label: "Finalizing session".to_string(),
                progress_pct: 100,
                terminal: false,
                playing: false,
                process_live: false,
                can_stop: false,
            }
        );
    }

    #[test]
    fn serialized_public_launch_status_includes_backend_view_model() {
        let payload = serde_json::to_value(public_launch_status(&RevisionedLaunchStatus::new(
            "session",
            4,
            LaunchStatusEvent {
                state: "monitoring".to_string(),
                benchmark: None,
                pid: Some(42),
                exit_code: None,
                failure_class: None,
                failure_detail: None,
                crash_evidence: None,
                healing: None,
                guardian: None,
                outcome: None,
                notice: None,
                evidence: Vec::new(),
                stages: Vec::new(),
            },
        )))
        .expect("serialize public launch status");

        assert_eq!(payload["session_id"], "session");
        assert_eq!(payload["revision"], 4);
        assert_eq!(payload["view_model"]["state_id"], "monitoring");
        assert_eq!(payload["view_model"]["label"], "Monitoring startup");
        assert_eq!(payload["view_model"]["progress_pct"], 88);
        assert_eq!(payload["view_model"]["process_live"], true);
        assert_eq!(payload["view_model"]["can_stop"], true);
    }

    #[test]
    fn public_launch_status_preserves_identity_and_safe_fields_during_redaction() {
        let session_id = "7d444840-9dc0-4a0c-a214-1e5a7a92533d";
        let crash_evidence: CrashEvidence = serde_json::from_value(serde_json::json!({
            "source": "minecraft_crash_report",
            "truncated": false,
            "failure_phase": "startup",
            "exception_class": "java.lang.OutOfMemoryError",
            "suspected_mods": [],
            "names_out_of_memory": true
        }))
        .expect("valid crash evidence");
        let payload = serde_json::to_value(public_launch_status(&RevisionedLaunchStatus::new(
            session_id,
            17,
            LaunchStatusEvent {
                state: "failed".to_string(),
                benchmark: Some(serde_json::json!({
                    "profile": "managed",
                    "unsafe_path": "/home/alice/.minecraft"
                })),
                pid: Some(42),
                exit_code: Some(1),
                failure_class: Some("startup_failed".to_string()),
                failure_detail: Some("java path C:\\Users\\Alice\\java.exe".to_string()),
                crash_evidence: Some(crash_evidence),
                healing: Some(serde_json::json!({
                    "decision": "retry",
                    "unsafe_path": "/home/alice/.minecraft"
                })),
                guardian: Some(serde_json::json!({
                    "decision": "blocked",
                    "message": "Guardian stopped an unsafe launch."
                })),
                outcome: Some(LaunchSessionOutcome::from_reason(
                    LaunchSessionExitReason::StartupFailed,
                )),
                notice: Some(LaunchNotice {
                    message: "Minecraft did not finish startup.".to_string(),
                    detail: Some("C:\\Users\\Alice\\AppData\\secret.txt".to_string()),
                    details: vec![
                        "Guardian retained the failure proof.".to_string(),
                        "/home/alice/.minecraft/accessToken".to_string(),
                    ],
                    tone: LaunchNoticeTone::Error,
                }),
                evidence: vec![LaunchStageEvidence {
                    id: "startup_failure".to_string(),
                    system: "guardian".to_string(),
                    summary: "Startup failure was classified.".to_string(),
                    details: vec![
                        "The process exited before boot.".to_string(),
                        "/home/alice/.minecraft/latest.log".to_string(),
                    ],
                }],
                stages: vec![LaunchStageRecord {
                    stage: "monitoring".to_string(),
                    label: "Monitoring startup".to_string(),
                    started_at_ms: 10,
                    ended_at_ms: Some(20),
                    duration_ms: Some(10),
                    result: Some("failed".to_string()),
                    warnings: vec![
                        "Startup ended early.".to_string(),
                        "C:\\Users\\Alice\\latest.log".to_string(),
                    ],
                    fallback_reason: None,
                    evidence: Vec::new(),
                }],
            },
        )))
        .expect("serialize public launch status");

        assert_eq!(payload["session_id"], session_id);
        assert_eq!(payload["revision"], 17);
        assert_eq!(payload["benchmark"]["profile"], "managed");
        assert!(payload["benchmark"].get("unsafe_path").is_none());
        assert_eq!(payload["healing"]["decision"], "retry");
        assert!(payload["healing"].get("unsafe_path").is_none());
        assert_eq!(payload["guardian"]["decision"], "blocked");
        assert_eq!(payload["failure_class"], "startup_failed");
        assert_eq!(payload["failure_detail"], serde_json::Value::Null);
        assert_eq!(payload["crash_evidence"]["failure_phase"], "startup");
        assert_eq!(payload["outcome"]["reason"], "startup_failed");
        assert_eq!(payload["notice"]["detail"], serde_json::Value::Null);
        assert_eq!(
            payload["notice"]["details"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            payload["evidence"][0]["details"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            payload["stages"][0]["warnings"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(payload["view_model"]["terminal"], true);
    }

    #[tokio::test]
    async fn recovering_status_payload_and_public_projection_have_snapshot_parity() {
        let root = unique_test_dir("recovering-status-payload");
        let state = test_app_state(&root);
        let session_id = "recovering-status";
        let mut record = test_record(session_id);
        record.state = LaunchState::Recovering;
        record.guardian = Some(serde_json::json!({
            "mode": "managed",
            "decision": "warned",
            "message": "Guardian is recovering this startup."
        }));
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert recovering session");

        let payload = serde_json::to_value(
            launch_status(&state, session_id)
                .await
                .expect("recovering launch status payload"),
        )
        .expect("serialize recovering payload");
        assert_eq!(payload["state"], "recovering");
        assert_eq!(payload["notice"], serde_json::Value::Null);
        assert_eq!(payload["outcome"], serde_json::Value::Null);
        assert_eq!(payload["view_model"]["terminal"], false);

        let status = state
            .sessions()
            .status_snapshot(session_id)
            .await
            .expect("recovering session snapshot");
        let public = serde_json::to_value(public_launch_status(&status))
            .expect("serialize public recovering status");
        assert_eq!(public["state"], payload["state"]);
        assert_eq!(public["notice"], payload["notice"]);
        assert_eq!(public["outcome"], payload["outcome"]);
        assert_eq!(public["view_model"]["terminal"], false);

        let _ = fs::remove_dir_all(root);
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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

    fn terminal_status() -> LaunchStatusEvent {
        LaunchStatusEvent {
            state: "exited".to_string(),
            benchmark: None,
            pid: None,
            exit_code: Some(0),
            failure_class: None,
            failure_detail: None,
            crash_evidence: None,
            healing: None,
            guardian: None,
            outcome: None,
            notice: None,
            evidence: Vec::new(),
            stages: Vec::new(),
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
