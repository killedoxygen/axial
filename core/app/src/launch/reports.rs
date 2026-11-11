//! Sanitized launch proof reports, their durable store and comparison baselines.

mod comparison;
#[cfg(test)]
mod tests;

pub(crate) use comparison::comparison_baseline_matches_report;
use comparison::*;

use serde::{Deserialize, Serialize};

use super::crash::CrashEvidence;
use super::logs::{LogEntry, MAX_LOG_ENTRIES, MAX_LOG_LINE_CHARS, Redactor};
use super::outcome::{SessionOutcome, SessionOutcomeKind};
use crate::storage::{
    MetadataStore, Migration, StorageError,
    rusqlite::{self, OptionalExtension, Transaction, params},
};
use std::sync::Arc;

pub const MAX_REPORT_BYTES: usize = 256 * 1024;
pub const MAX_RECENT_REPORTS: usize = 25;

pub const REPORT_MIGRATION: Migration = Migration {
    id: "launch_reports.v1",
    sql: "CREATE TABLE launch_reports (
        session_id TEXT PRIMARY KEY NOT NULL CHECK(length(session_id) BETWEEN 1 AND 96),
        instance_id TEXT NOT NULL CHECK(length(instance_id) BETWEEN 1 AND 96),
        recorded_at TEXT NOT NULL CHECK(length(recorded_at) = 24),
        payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 262144)
    ) STRICT;
    CREATE INDEX launch_reports_recent ON launch_reports(recorded_at DESC, session_id DESC);",
};

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("launch report metadata is invalid")]
    Invalid,
    #[error("launch report exceeds the supported size")]
    TooLarge,
    #[error("a different terminal report already exists for this session")]
    ConflictingSession,
    #[error("launch report storage is unavailable")]
    Storage(#[from] StorageError),
}

impl From<rusqlite::Error> for ReportError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofStageEvidence {
    pub id: String,
    pub system: String,
    pub summary: String,
    #[serde(default)]
    pub details: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofStage {
    pub stage: String,
    pub label: String,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub result: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub fallback_reason: Option<String>,
    #[serde(default)]
    pub evidence: Vec<LaunchProofStageEvidence>,
}

/// Schema 4 removes Guardian/healing fields and includes sanitized historical output.
/// IDs identify metadata only and never grant authority to a filesystem location.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofRecord {
    pub schema: String,
    pub schema_version: u32,
    pub session_id: String,
    pub instance_id: String,
    pub version_id: String,
    pub launched_at: String,
    pub recorded_at: String,
    pub outcome: String,
    pub session_outcome: SessionOutcome,
    pub scenario: LaunchProofScenario,
    pub device: LaunchProofDevice,
    pub resource_budget: Option<LaunchProofResourceBudget>,
    pub exit_code: Option<i32>,
    pub boot_duration_ms: Option<u64>,
    pub crash_evidence: Option<CrashEvidence>,
    pub stages: Vec<LaunchProofStage>,
    pub comparison: Option<LaunchProofComparison>,
    pub logs: Vec<LogEntry>,
    pub logs_dropped: u64,
}

/// Process owners provide facts they observed. Additional scenario/device/stage
/// evidence can be supplied through `record` once its feature owner has captured it.
pub struct SessionReportInput {
    pub session_id: String,
    pub instance_id: String,
    pub version_id: String,
    pub launched_at: String,
    pub ended_at: String,
    pub outcome: SessionOutcome,
    pub entries: Vec<LogEntry>,
    pub exit_code: Option<i32>,
    pub boot_duration_ms: Option<u64>,
    pub logs_dropped: u64,
}

impl LaunchProofRecord {
    pub fn from_session(input: SessionReportInput) -> Self {
        Self {
            schema: "axial.launch.proof".to_owned(),
            schema_version: 4,
            session_id: input.session_id,
            instance_id: input.instance_id,
            version_id: input.version_id,
            launched_at: input.launched_at,
            recorded_at: input.ended_at,
            outcome: outcome_name(&input.outcome).to_owned(),
            session_outcome: input.outcome,
            scenario: LaunchProofScenario::default(),
            device: LaunchProofDevice::default(),
            resource_budget: None,
            exit_code: input.exit_code,
            boot_duration_ms: input.boot_duration_ms,
            crash_evidence: None,
            stages: Vec::new(),
            comparison: None,
            logs: input.entries,
            logs_dropped: input.logs_dropped,
        }
    }
}

fn outcome_name(outcome: &SessionOutcome) -> &'static str {
    match outcome.kind {
        SessionOutcomeKind::Clean => "exited",
        SessionOutcomeKind::Stopped => "stopped",
        SessionOutcomeKind::Failed => "failed",
        SessionOutcomeKind::Unknown => "unknown",
    }
}

/// Recent launch proofs with their history view models.
#[derive(Clone, Debug, Serialize)]
pub struct LaunchReportsResponse {
    pub reports: Vec<serde_json::Value>,
}

/// Synchronous, bounded metadata methods. Async adapters run them on spawn_blocking.
#[derive(Clone)]
pub struct LaunchReportStore {
    metadata: Arc<MetadataStore>,
}

fn stored_report(
    tx: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<LaunchProofRecord>, ReportError> {
    stored_report_row(tx, session_id, MAX_REPORT_BYTES)?
        .map(|(instance_id, recorded_at, bytes)| {
            decode_stored_report(
                session_id,
                &instance_id,
                &recorded_at,
                &bytes.ok_or(ReportError::TooLarge)?,
            )
        })
        .transpose()
}

fn stored_report_row(
    db: &rusqlite::Connection,
    session_id: &str,
    max_bytes: usize,
) -> Result<Option<(String, String, Option<Vec<u8>>)>, ReportError> {
    Ok(db
        .query_row(
            "SELECT instance_id, recorded_at, CASE WHEN length(payload)<=?2 THEN payload END
             FROM launch_reports WHERE session_id = ?1",
            params![session_id, max_bytes],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

fn decode_stored_report(
    session_id: &str,
    instance_id: &str,
    recorded_at: &str,
    bytes: &[u8],
) -> Result<LaunchProofRecord, ReportError> {
    let saved = decode_report(bytes)?;
    let original: LaunchProofRecord =
        serde_json::from_slice(bytes).map_err(|_| ReportError::Invalid)?;
    if saved.session_id != session_id
        || saved.instance_id != instance_id
        || saved.recorded_at != recorded_at
        || saved != original
    {
        return Err(ReportError::Invalid);
    }
    Ok(saved)
}

fn insert_report(
    tx: &Transaction<'_>,
    report: &LaunchProofRecord,
    payload: &[u8],
) -> Result<(), ReportError> {
    let changed = tx.execute(
        "INSERT INTO launch_reports(session_id, instance_id, recorded_at, payload) VALUES (?1, ?2, ?3, ?4)",
        params![report.session_id, report.instance_id, report.recorded_at, payload],
    )?;
    if changed != 1 {
        return Err(ReportError::Invalid);
    }
    Ok(())
}

impl LaunchReportStore {
    pub fn new(metadata: Arc<MetadataStore>) -> Result<Self, ReportError> {
        metadata.migrate(&[REPORT_MIGRATION])?;
        Ok(Self { metadata })
    }

    /// Already-redacted collector entries are rechecked at the durable boundary.
    pub fn record_session(&self, input: SessionReportInput) -> Result<(), ReportError> {
        self.record(
            LaunchProofRecord::from_session(input),
            &Redactor::new(Vec::new()),
        )
    }

    pub fn record(
        &self,
        mut report: LaunchProofRecord,
        redactor: &Redactor,
    ) -> Result<(), ReportError> {
        sanitize_report(&mut report, redactor)?;
        // Comparison is derived locally from committed reports, never caller-authored.
        report.comparison = None;
        self.metadata.transaction(|tx| -> Result<(), ReportError> {
            if let Some(mut saved) = stored_report(tx, &report.session_id)? {
                saved.comparison = None;
                if saved != report { return Err(ReportError::ConflictingSession); }
                return super::coordinator::acknowledge_terminal(tx, &saved);
            }
            let mut statement = tx.prepare(
                "SELECT payload FROM launch_reports ORDER BY recorded_at DESC, session_id DESC LIMIT 100",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut candidates = Vec::new();
            for row in rows {
                let mut candidate = decode_report(&row?)?;
                candidate.logs.clear();
                candidates.push(candidate);
            }
            report.comparison = build_comparison_from_candidates(&report, &candidates);
            let payload = encode_report(&report)?;
            insert_report(tx, &report, &payload)?;
            let saved = stored_report(tx, &report.session_id)?.ok_or(ReportError::Invalid)?;
            if saved != report { return Err(ReportError::Invalid); }
            super::coordinator::acknowledge_terminal(tx, &saved)?;
            Ok(())
        })
    }

    pub fn get(&self, session_id: &str) -> Result<Option<LaunchProofRecord>, ReportError> {
        self.metadata
            .read(|connection| Self::get_in(connection, session_id))
    }

    pub(crate) fn get_in(
        connection: &rusqlite::Connection,
        session_id: &str,
    ) -> Result<Option<LaunchProofRecord>, ReportError> {
        if !valid_report_id(session_id) {
            return Err(ReportError::Invalid);
        }
        stored_report(connection, session_id)
    }

    pub(super) fn acknowledge_intent(&self, session_id: &str) -> Result<(), ReportError> {
        self.metadata.transaction(|tx| {
            let report = stored_report(tx, session_id)?.ok_or(ReportError::Invalid)?;
            super::coordinator::acknowledge_terminal(tx, &report)
        })
    }

    pub fn list_recent(&self, limit: usize) -> Result<Vec<LaunchProofRecord>, ReportError> {
        self.metadata.read(|connection| -> Result<_, ReportError> {
            let mut statement = connection.prepare(
                "SELECT session_id, payload FROM launch_reports ORDER BY recorded_at DESC, session_id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map([limit.min(MAX_RECENT_REPORTS)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut reports = Vec::new();
            for row in rows {
                let (id, bytes) = row?;
                let report = decode_report(&bytes)?;
                if report.session_id != id { return Err(ReportError::Invalid); }
                reports.push(report);
            }
            Ok(reports)
        })
    }
}

pub fn encode_report(report: &LaunchProofRecord) -> Result<Vec<u8>, ReportError> {
    let mut report = report.clone();
    sanitize_report(&mut report, &Redactor::new(Vec::new()))?;
    let bytes = serde_json::to_vec(&report).map_err(|_| ReportError::Invalid)?;
    if bytes.len() > MAX_REPORT_BYTES {
        return Err(ReportError::TooLarge);
    }
    Ok(bytes)
}

pub fn decode_report(bytes: &[u8]) -> Result<LaunchProofRecord, ReportError> {
    if bytes.len() > MAX_REPORT_BYTES {
        return Err(ReportError::TooLarge);
    }
    let mut report: LaunchProofRecord =
        serde_json::from_slice(bytes).map_err(|_| ReportError::Invalid)?;
    sanitize_report(&mut report, &Redactor::new(Vec::new()))?;
    Ok(report)
}

/// Historical report keys are not live-session capabilities.
pub fn valid_report_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

fn safe_token(value: &str, redactor: &Redactor) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-+".contains(&c))
        && redactor.redact_line(value) == value)
        .then(|| value.to_owned())
}

fn safe_version_id(value: &str, redactor: &Redactor) -> Option<String> {
    if !value.starts_with("loader-v2-") {
        return safe_token(value, redactor);
    }
    let identity = axial_minecraft::loaders::api::decode_installed_version_id(value).ok()?;
    if redactor.contains_secret(value) {
        return None;
    }
    // The canonical encoding resembles a secret in unstructured logs. Only
    // typed version fields may retain it, after checking decoded coordinates.
    for coordinate in [identity.minecraft_version(), identity.loader_version()] {
        if redactor.redact_line(coordinate) != coordinate {
            return None;
        }
    }
    Some(value.to_owned())
}

fn safe_detail(value: &str, redactor: &Redactor) -> Option<String> {
    let safe = redactor.redact_line(value);
    (safe == value && safe.chars().count() <= 180).then_some(safe)
}

fn canonical_time(value: &str) -> Result<String, ReportError> {
    let value = chrono::DateTime::parse_from_rfc3339(value).map_err(|_| ReportError::Invalid)?;
    let value = value.with_timezone(&chrono::Utc);
    let text = value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if text.len() != 24 {
        return Err(ReportError::Invalid);
    }
    Ok(text)
}

fn sanitize_report(report: &mut LaunchProofRecord, redactor: &Redactor) -> Result<(), ReportError> {
    if report.schema != "axial.launch.proof"
        || report.schema_version != 4
        || !valid_report_id(&report.session_id)
        || !valid_report_id(&report.instance_id)
        || redactor.contains_secret(&report.session_id)
        || redactor.contains_secret(&report.instance_id)
    {
        return Err(ReportError::Invalid);
    }
    report.launched_at = canonical_time(&report.launched_at)?;
    report.recorded_at = canonical_time(&report.recorded_at)?;
    if report.recorded_at < report.launched_at {
        return Err(ReportError::Invalid);
    }
    report.version_id =
        safe_version_id(&report.version_id, redactor).unwrap_or_else(|| "unknown".into());
    report.outcome = outcome_name(&report.session_outcome).to_owned();
    report.session_outcome.summary = report.session_outcome.summary().to_owned();
    report.scenario.performance_mode =
        safe_token(&report.scenario.performance_mode, redactor).unwrap_or_else(|| "unknown".into());
    report.scenario.scenario_id =
        scenario_id_for_performance_mode(&report.scenario.performance_mode).into();
    report.scenario.version_id = report
        .scenario
        .version_id
        .as_deref()
        .and_then(|value| safe_version_id(value, redactor));
    for token in [
        &mut report.scenario.benchmark_profile,
        &mut report.scenario.benchmark_run_type,
        &mut report.scenario.benchmark_mode,
        &mut report.scenario.benchmark_id,
    ] {
        *token = token
            .as_deref()
            .and_then(|value| safe_token(value, redactor));
    }
    report.device.tier =
        safe_token(&report.device.tier, redactor).unwrap_or_else(|| "unknown".into());
    report.stages.truncate(32);
    for stage in &mut report.stages {
        stage.stage = safe_token(&stage.stage, redactor).unwrap_or_else(|| "unknown".into());
        stage.label = safe_detail(&stage.label, redactor).unwrap_or_else(|| "Launch stage".into());
        stage.result = stage
            .result
            .as_deref()
            .and_then(|value| safe_token(value, redactor));
        stage.fallback_reason = stage
            .fallback_reason
            .as_deref()
            .and_then(|value| safe_detail(value, redactor));
        stage.warnings = stage
            .warnings
            .iter()
            .filter_map(|value| safe_detail(value, redactor))
            .take(8)
            .collect();
        stage.evidence = stage
            .evidence
            .iter()
            .filter_map(|evidence| {
                Some(LaunchProofStageEvidence {
                    id: safe_token(&evidence.id, redactor)?,
                    system: safe_token(&evidence.system, redactor)?,
                    summary: safe_detail(&evidence.summary, redactor)?,
                    details: evidence
                        .details
                        .iter()
                        .filter_map(|value| safe_detail(value, redactor))
                        .take(4)
                        .collect(),
                })
            })
            .take(4)
            .collect();
        if let Some(end) = stage.ended_at_ms {
            if end < stage.started_at_ms {
                return Err(ReportError::Invalid);
            }
            stage.duration_ms = Some(end - stage.started_at_ms);
        } else {
            stage.duration_ms = None;
        }
    }
    if report.crash_evidence.as_ref().is_some_and(|evidence| {
        serde_json::to_string(evidence).is_ok_and(|value| redactor.contains_secret(&value))
    }) {
        report.crash_evidence = None;
    }
    let mut previous = 0;
    for entry in &mut report.logs {
        if entry.sequence <= previous {
            return Err(ReportError::Invalid);
        }
        previous = entry.sequence;
        entry.truncated |= entry.text.chars().count() > MAX_LOG_LINE_CHARS;
        entry.text = redactor.redact_line(&entry.text);
    }
    let mut log_bytes: usize = report.logs.iter().map(|entry| entry.text.len() + 100).sum();
    let mut remove = report.logs.len().saturating_sub(MAX_LOG_ENTRIES);
    log_bytes = log_bytes.saturating_sub(
        report.logs[..remove]
            .iter()
            .map(|entry| entry.text.len() + 100)
            .sum(),
    );
    while log_bytes > MAX_REPORT_BYTES / 3 && remove < report.logs.len() {
        log_bytes = log_bytes.saturating_sub(report.logs[remove].text.len() + 100);
        remove += 1;
    }
    report.logs.drain(..remove);
    report.logs_dropped = report.logs_dropped.saturating_add(remove as u64);
    sanitize_comparison(report, redactor)?;
    Ok(())
}

fn sanitize_comparison(
    report: &mut LaunchProofRecord,
    redactor: &Redactor,
) -> Result<(), ReportError> {
    let Some(comparison) = &mut report.comparison else {
        return Ok(());
    };
    if !valid_report_id(&comparison.baseline_session_id)
        || comparison.baseline_session_id == report.session_id
        || redactor.contains_secret(&comparison.baseline_session_id)
        || comparison.baseline_value_ms == 0
        || comparison.matched_sample_count == 0
        || !comparison.delta_percent.is_finite()
        || !matches!(
            comparison.metric_name.as_str(),
            LAUNCH_BOOT_COMPARISON_METRIC_NAME | LAUNCH_STAGE_COMPARISON_METRIC_NAME
        )
    {
        return Err(ReportError::Invalid);
    }
    comparison.baseline_recorded_at = canonical_time(&comparison.baseline_recorded_at)?;
    comparison.baseline.version_id =
        safe_version_id(&comparison.baseline.version_id, redactor).ok_or(ReportError::Invalid)?;
    for token in [
        &mut comparison.baseline.performance_mode,
        &mut comparison.baseline.device_tier,
    ] {
        *token = safe_token(token, redactor).ok_or(ReportError::Invalid)?;
    }
    for token in [
        &mut comparison.baseline.benchmark_profile,
        &mut comparison.baseline.benchmark_run_type,
        &mut comparison.baseline.benchmark_mode,
    ] {
        *token = token
            .as_deref()
            .and_then(|value| safe_token(value, redactor));
    }
    comparison.delta_ms =
        metric_delta_ms(comparison.current_value_ms, comparison.baseline_value_ms);
    comparison.delta_percent =
        (comparison.delta_ms as f64 / comparison.baseline_value_ms as f64) * 100.0;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofScenario {
    pub scenario_id: String,
    pub performance_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_memory_mb: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<String>,
}

impl Default for LaunchProofScenario {
    fn default() -> Self {
        Self {
            scenario_id: "unknown_launch".to_string(),
            performance_mode: "unknown".to_string(),
            requested_memory_mb: None,
            version_id: None,
            benchmark_profile: None,
            benchmark_run_type: None,
            benchmark_mode: None,
            benchmark_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofDevice {
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_threads: Option<usize>,
}

impl Default for LaunchProofDevice {
    fn default() -> Self {
        Self {
            tier: "unknown".to_string(),
            total_memory_mb: None,
            cpu_threads: None,
        }
    }
}

impl LaunchProofDevice {
    pub(super) fn from_budget(budget: &LaunchProofResourceBudget) -> Self {
        let cpu_threads = budget.host_cpu_threads.filter(|threads| *threads > 0);
        let total_memory_mb = budget.host_total_memory_mb.filter(|memory| *memory > 0);
        let cpu_tier = cpu_threads.map(|threads| match threads {
            0..=4 => 0,
            8.. => 2,
            _ => 1,
        });
        let memory_tier = total_memory_mb.map(|memory| match memory {
            0..=8192 => 0,
            32768.. => 2,
            _ => 1,
        });
        let tier = cpu_tier
            .into_iter()
            .chain(memory_tier)
            .min()
            .map_or("unknown", |tier| ["low", "mid", "high"][tier]);
        Self {
            tier: tier.into(),
            total_memory_mb,
            cpu_threads,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofComparison {
    pub baseline_session_id: String,
    pub baseline_recorded_at: String,
    pub baseline: LaunchProofComparisonBaseline,
    pub matched_sample_count: usize,
    pub metric_name: String,
    pub current_value_ms: u64,
    pub baseline_value_ms: u64,
    pub delta_ms: i64,
    pub delta_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofComparisonBaseline {
    pub performance_mode: String,
    pub version_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_memory_mb: Option<i32>,
    pub device_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofResourceBudget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_total_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_available_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_used_memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_threads: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_load_1m_x100: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_load_5m_x100: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_load_15m_x100: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launcher_process_memory_mb: Option<u64>,
    pub active_session_count: usize,
    pub active_install_count: usize,
    pub active_memory_allocation_mb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_memory_mb: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_remaining_memory_mb: Option<i64>,
    pub memory_headroom_mb: u64,
    pub memory_pressure: bool,
    pub cpu_pressure: bool,
    pub install_pressure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_disk_available_mb: Option<u64>,
    pub launch_disk_headroom_mb: u64,
    pub disk_pressure: bool,
}
