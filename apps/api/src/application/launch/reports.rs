use super::runner::persist_launch_proof_owned;
use super::trace_launch_event;
use crate::observability::{
    RedactionAudience, sanitize_public_diagnostic_text, sanitize_public_json_value,
};
use crate::state::{AppState, LaunchStatusEvent};
use axial_launcher::{
    GuardianDecision, GuardianSummary, LaunchHealingSummary, LaunchSessionRecord, snapshot_status,
};
use axum::Json;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const LAUNCH_COMMAND_REDACTED_VALUE: &str = "<redacted>";
pub(crate) const LAUNCH_KILL_INTERNAL_ERROR_MESSAGE: &str =
    "Could not stop the launch session. Try again from the launcher.";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchStatusViewModel {
    pub state_id: String,
    pub label: String,
    pub progress_pct: u8,
    pub terminal: bool,
}

impl LaunchStatusViewModel {
    pub fn for_state(state: &str) -> Self {
        let (state_id, label, progress_pct, terminal) = launch_status_view_fields(state);
        Self {
            state_id: state_id.to_string(),
            label: label.to_string(),
            progress_pct,
            terminal,
        }
    }
}

fn launch_status_view_fields(state: &str) -> (&'static str, &'static str, u8, bool) {
    match state {
        "queued" => ("queued", "Preparing launch", 8, false),
        "planning" => ("planning", "Planning launch", 18, false),
        "validating" => ("validating", "Validating launch", 24, false),
        "ensuring_runtime" => ("ensuring_runtime", "Ensuring runtime", 34, false),
        "downloading_runtime" => ("downloading_runtime", "Downloading runtime", 42, false),
        "preparing" => ("preparing", "Preparing files", 56, false),
        "prewarming" => ("prewarming", "Prewarming game data", 64, false),
        "starting" => ("starting", "Starting process", 72, false),
        "monitoring" => ("monitoring", "Monitoring startup", 88, false),
        "running" => ("running", "Running", 100, false),
        "degraded" => ("degraded", "Running with warnings", 100, false),
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
    let detail = first_bounded_public_detail(
        guardian
            .message
            .iter()
            .cloned()
            .chain(guardian.details.iter().cloned())
            .chain(guardian.guidance.iter().cloned())
            .chain(
                guardian
                    .interventions
                    .iter()
                    .filter_map(|intervention| intervention.detail.clone()),
            ),
    );
    let has_guardian_action = matches!(
        guardian.decision,
        GuardianDecision::Blocked | GuardianDecision::Warned | GuardianDecision::Intervened
    );
    if !has_guardian_action && detail.is_none() {
        return None;
    }

    Some(json!({
        "tone": guardian_decision_tone(guardian.decision),
        "label": guardian_decision_label(guardian.decision),
        "detail": detail,
    }))
}

fn guardian_decision_label(decision: GuardianDecision) -> &'static str {
    match decision {
        GuardianDecision::Blocked => "Guardian blocked",
        GuardianDecision::Warned => "Guardian warned",
        GuardianDecision::Intervened => "Guardian intervened",
        GuardianDecision::Allowed => "Guardian note",
    }
}

fn guardian_decision_tone(decision: GuardianDecision) -> &'static str {
    match decision {
        GuardianDecision::Blocked => "err",
        GuardianDecision::Warned => "warn",
        GuardianDecision::Intervened | GuardianDecision::Allowed => "info",
    }
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

pub(crate) async fn launch_status_payload(
    state: &AppState,
    id: &str,
) -> Result<serde_json::Value, super::LaunchApplicationError> {
    let record = state.sessions().get(id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    let status = snapshot_status(&record);
    let mut response = json!({
        "state": status.state,
        "pid": status.pid,
        "exit_code": status.exit_code,
        "failure_class": status.failure_class,
        "failure_detail": status.failure_detail,
        "healing": status.healing,
        "guardian": status.guardian,
        "outcome": status.outcome,
        "notice": status.notice,
        "stages": status.stages,
        "session_id": record.session_id.0,
        "view_model": LaunchStatusViewModel::for_state(&status.state),
    });
    if let Some(benchmark) = status.benchmark {
        response["benchmark"] = benchmark;
    }

    Ok(public_payload(response))
}

pub fn public_launch_status_json(status: &LaunchStatusEvent) -> serde_json::Value {
    let mut payload = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    payload["view_model"] = json!(LaunchStatusViewModel::for_state(&status.state));
    public_payload(payload)
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

    trace_launch_event(id, "kill requested by client");
    stop.emit_log("system", "Launch stopped by user.").await;
    stop.emit_status(LaunchStatusEvent {
        state: "exited".to_string(),
        benchmark: None,
        pid: record.pid,
        exit_code: Some(-9),
        failure_class: None,
        failure_detail: Some("stopped by user".to_string()),
        healing: record.healing.clone(),
        guardian: record.guardian.clone(),
        outcome: None,
        notice: None,
        evidence: Vec::new(),
        stages: Vec::new(),
    })
    .await;
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

pub(crate) fn launch_kill_error_response(error: std::io::Error) -> super::LaunchApplicationError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        );
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": LAUNCH_KILL_INTERNAL_ERROR_MESSAGE })),
    )
}

#[cfg(test)]
mod tests {
    use super::{LaunchStatusViewModel, public_launch_status_json, stop_launch_session};
    use crate::state::{AppState, AppStateInit, InstallStore, LaunchStatusEvent, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceStore};
    use axial_launcher::{LaunchSessionRecord, LaunchState, SessionId};
    use axial_performance::PerformanceManager;
    use axum::http::StatusCode;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn stop_launch_session_releases_its_retention_hold_when_kill_fails() {
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
        assert_eq!(error.0, StatusCode::NOT_FOUND);

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

    #[test]
    fn launch_status_view_model_authors_progress_copy() {
        assert_eq!(
            LaunchStatusViewModel::for_state("validating"),
            LaunchStatusViewModel {
                state_id: "validating".to_string(),
                label: "Validating launch".to_string(),
                progress_pct: 24,
                terminal: false,
            }
        );
        assert_eq!(
            LaunchStatusViewModel::for_state("exited"),
            LaunchStatusViewModel {
                state_id: "exited".to_string(),
                label: "Exited".to_string(),
                progress_pct: 100,
                terminal: true,
            }
        );
    }

    #[test]
    fn public_launch_status_json_includes_backend_view_model() {
        let payload = public_launch_status_json(&LaunchStatusEvent {
            state: "monitoring".to_string(),
            benchmark: None,
            pid: None,
            exit_code: None,
            failure_class: None,
            failure_detail: None,
            healing: None,
            guardian: None,
            outcome: None,
            notice: None,
            evidence: Vec::new(),
            stages: Vec::new(),
        });

        assert_eq!(payload["view_model"]["state_id"], "monitoring");
        assert_eq!(payload["view_model"]["label"], "Monitoring startup");
        assert_eq!(payload["view_model"]["progress_pct"], 88);
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
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
