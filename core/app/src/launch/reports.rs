//! Bounded neutral crash evidence retained from the proven launcher leaf parser.
//! Artifact bytes must be admitted by the instance resource owner; this module never reads paths.

use serde::{Deserialize, Deserializer, Serialize, de};

use super::logs::{LogEntry, MAX_LOG_ENTRIES, MAX_LOG_LINE_CHARS, Redactor};
use super::outcome::{SessionOutcome, SessionOutcomeKind};
use crate::storage::{
    MetadataStore, Migration, StorageError,
    rusqlite::{self, OptionalExtension, Transaction, params},
};
use std::sync::Arc;

pub const MAX_REPORT_BYTES: usize = 256 * 1024;
pub const MAX_RECENT_REPORTS: usize = 25;
const LAUNCH_STAGE_COMPARISON_METRIC_NAME: &str = "total_completed_stage_duration_ms";
const LAUNCH_BOOT_COMPARISON_METRIC_NAME: &str = "boot_duration_ms";
type LaunchComparisonMetric = (&'static str, u64, fn(&LaunchProofRecord) -> Option<u64>);

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

    pub(crate) fn matches_imported_terminal_state(&self, state: &str) -> bool {
        // The converter appends this block after preserved predecessor stages.
        let start = self.stages.iter().rposition(|stage| {
            stage.stage == "imported_history"
                && stage
                    .evidence
                    .iter()
                    .any(|item| item.system == "history" && item.id == "original_session")
        });
        let Some(start) = start else {
            return state == self.outcome && matches!(state, "exited" | "failed" | "stopped");
        };
        let mut original = None;
        let mut reason = false;
        for stage in &self.stages[start..] {
            if stage.stage != "imported_history" {
                return false;
            }
            for item in &stage.evidence {
                if item.system != "history" {
                    return false;
                }
                match item.id.as_str() {
                    "original_outcome" if original.is_none() && item.details.len() == 1 => {
                        original = Some(item.details[0].as_str())
                    }
                    "original_reason" if !reason && item.details.len() == 1 => reason = true,
                    "original_outcome" | "original_reason" => return false,
                    _ => {}
                }
            }
        }
        let Some(original) = original else {
            return false;
        };
        if !imported_outcome_matches(original, reason.then_some(self.session_outcome.kind)) {
            return false;
        }
        let (original, terminal) =
            imported_suite_states(original, reason.then_some(self.session_outcome.kind));
        state == original || state == terminal
    }
}

/// The predecessor suite records either its report outcome or its terminal
/// observer's classification. Normalizing a report must not erase that choice.
pub(crate) fn imported_suite_states(
    original: &str,
    kind: Option<SessionOutcomeKind>,
) -> (String, String) {
    (
        match original {
            "failed" | "stopped" | "exited" | "completed" => original,
            _ => "failed",
        }
        .into(),
        match kind {
            Some(SessionOutcomeKind::Clean | SessionOutcomeKind::Unknown) => "exited",
            Some(SessionOutcomeKind::Stopped) => "stopped",
            Some(SessionOutcomeKind::Failed) => "failed",
            None => original,
        }
        .into(),
    )
}

pub(crate) fn imported_outcome_matches(outcome: &str, kind: Option<SessionOutcomeKind>) -> bool {
    match (outcome, kind) {
        ("failed" | "exited" | "completed" | "stopped" | "cancelled" | "canceled", None) => true,
        ("exited", Some(_)) => true,
        ("failed", Some(SessionOutcomeKind::Failed | SessionOutcomeKind::Unknown)) => true,
        ("completed", Some(SessionOutcomeKind::Clean)) => true,
        ("stopped" | "cancelled" | "canceled", Some(SessionOutcomeKind::Stopped)) => true,
        ("unknown", Some(SessionOutcomeKind::Unknown)) => true,
        _ => false,
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

/// Synchronous, bounded metadata methods. Async adapters run them on spawn_blocking.
#[derive(Clone)]
pub struct LaunchReportStore {
    metadata: Arc<MetadataStore>,
}

/// Canonical historical records prepared before an instance publication transaction.
/// This value grants metadata insertion only, never session or process authority.
#[derive(Clone)]
pub(crate) struct PreparedReportImport {
    records: Vec<(LaunchProofRecord, Vec<u8>)>,
}

impl PreparedReportImport {
    pub(crate) fn prepare(reports: Vec<LaunchProofRecord>) -> Result<Self, ReportError> {
        let mut records = Vec::with_capacity(reports.len());
        let mut ids = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        if reports.len() > 1024 {
            return Err(ReportError::TooLarge);
        }
        for report in reports {
            let suffix = report
                .session_id
                .strip_prefix("legacy-")
                .ok_or(ReportError::Invalid)?;
            if suffix.len() != 64
                || !suffix
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                || !ids.insert(report.session_id.clone())
                || report
                    .instance_id
                    .parse::<crate::instances::model::InstanceId>()
                    .is_err()
            {
                return Err(ReportError::Invalid);
            }
            let encoded = encode_report(&report)?;
            // Import must never silently discard evidence to fit the current schema.
            if decode_report(&encoded)? != report {
                return Err(ReportError::Invalid);
            }
            bytes = bytes
                .checked_add(encoded.len())
                .ok_or(ReportError::TooLarge)?;
            if bytes > 64 * 1024 * 1024 {
                return Err(ReportError::TooLarge);
            }
            records.push((report, encoded));
        }
        Ok(Self { records })
    }

    pub(crate) fn insert_in(&self, tx: &Transaction<'_>) -> Result<(), ReportError> {
        for (report, encoded) in &self.records {
            match stored_report(tx, &report.session_id)? {
                Some(saved) if saved == *report => {}
                Some(_) => return Err(ReportError::ConflictingSession),
                None => insert_report(tx, report, encoded)?,
            }
        }
        Ok(())
    }

    pub(crate) fn verify_in(&self, tx: &Transaction<'_>) -> Result<(), ReportError> {
        for (report, _) in &self.records {
            if stored_report(tx, &report.session_id)?.as_ref() != Some(report) {
                return Err(ReportError::ConflictingSession);
            }
        }
        Ok(())
    }
}

fn stored_report(
    tx: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<LaunchProofRecord>, ReportError> {
    let existing: Option<(String, String, Vec<u8>)> = tx
        .query_row(
            "SELECT instance_id, recorded_at, payload FROM launch_reports WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    existing
        .map(|(instance_id, recorded_at, bytes)| {
            let saved = decode_report(&bytes)?;
            let original: LaunchProofRecord =
                serde_json::from_slice(&bytes).map_err(|_| ReportError::Invalid)?;
            if saved.session_id != session_id
                || saved.instance_id != instance_id
                || saved.recorded_at != recorded_at
                || saved != original
            {
                return Err(ReportError::Invalid);
            }
            Ok(saved)
        })
        .transpose()
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

fn build_comparison_from_candidates(
    current: &LaunchProofRecord,
    candidates: &[LaunchProofRecord],
) -> Option<LaunchProofComparison> {
    if !launch_proof_outcome_is_comparable(&current.outcome) {
        return None;
    }

    let (metric_name, current_value_ms, metric_value) =
        launch_comparison_metric_for_current(current)?;
    let mut matches = candidates
        .iter()
        .filter(|candidate| report_precedes(candidate, current))
        .filter(|candidate| launch_proof_outcome_is_comparable(&candidate.outcome))
        .filter(|candidate| comparison_dimensions_match(current, candidate))
        .filter_map(|candidate| {
            let value_ms = metric_value(candidate)?;
            (value_ms > 0).then_some((candidate, value_ms))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left, _), (right, _)| {
        comparison_baseline_mode_rank(current, left)
            .cmp(&comparison_baseline_mode_rank(current, right))
            .then_with(|| {
                right
                    .recorded_at
                    .cmp(&left.recorded_at)
                    .then_with(|| right.session_id.cmp(&left.session_id))
            })
    });
    let matched_sample_count = matches.len();
    let (baseline, baseline_value_ms) = matches.first()?;
    let delta_ms = metric_delta_ms(current_value_ms, *baseline_value_ms);

    Some(LaunchProofComparison {
        baseline_session_id: baseline.session_id.clone(),
        baseline_recorded_at: baseline.recorded_at.clone(),
        baseline: comparison_baseline_snapshot(baseline)?,
        matched_sample_count,
        metric_name: metric_name.to_string(),
        current_value_ms,
        baseline_value_ms: *baseline_value_ms,
        delta_ms,
        delta_percent: (delta_ms as f64 / *baseline_value_ms as f64) * 100.0,
    })
}

fn comparison_baseline_snapshot(
    report: &LaunchProofRecord,
) -> Option<LaunchProofComparisonBaseline> {
    Some(LaunchProofComparisonBaseline {
        performance_mode: known_launch_mode(report)?.to_string(),
        version_id: normalized_version_target(report)?.to_string(),
        requested_memory_mb: report.scenario.requested_memory_mb,
        device_tier: normalized_dimension(&report.device.tier)?.to_string(),
        benchmark_profile: report
            .scenario
            .benchmark_profile
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
        benchmark_run_type: report
            .scenario
            .benchmark_run_type
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
        benchmark_mode: report
            .scenario
            .benchmark_mode
            .as_deref()
            .and_then(normalized_dimension)
            .map(str::to_string),
    })
}

pub(crate) fn comparison_baseline_matches_report(
    comparison: &LaunchProofComparison,
    baseline: &LaunchProofRecord,
) -> bool {
    let metric_value = match comparison.metric_name.as_str() {
        LAUNCH_STAGE_COMPARISON_METRIC_NAME => launch_total_completed_stage_duration_ms(baseline),
        LAUNCH_BOOT_COMPARISON_METRIC_NAME => baseline.boot_duration_ms,
        _ => None,
    };
    launch_proof_outcome_is_comparable(&baseline.outcome)
        && comparison.baseline_session_id == baseline.session_id
        && comparison.baseline_recorded_at == baseline.recorded_at
        && comparison_baseline_snapshot(baseline).as_ref() == Some(&comparison.baseline)
        && metric_value == Some(comparison.baseline_value_ms)
}

fn comparison_baseline_mode_rank(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> u8 {
    match (known_launch_mode(current), known_launch_mode(candidate)) {
        (Some("managed"), Some("vanilla")) => 0,
        _ => 1,
    }
}

fn report_precedes(candidate: &LaunchProofRecord, current: &LaunchProofRecord) -> bool {
    (&candidate.recorded_at, &candidate.session_id) < (&current.recorded_at, &current.session_id)
}

fn launch_proof_outcome_is_comparable(outcome: &str) -> bool {
    matches!(outcome.trim(), "running" | "exited" | "completed")
}

fn launch_comparison_metric_for_current(
    current: &LaunchProofRecord,
) -> Option<LaunchComparisonMetric> {
    if let Some(boot_duration_ms) = current.boot_duration_ms {
        return Some((
            LAUNCH_BOOT_COMPARISON_METRIC_NAME,
            boot_duration_ms,
            launch_boot_duration_ms,
        ));
    }

    Some((
        LAUNCH_STAGE_COMPARISON_METRIC_NAME,
        launch_total_completed_stage_duration_ms(current)?,
        launch_total_completed_stage_duration_ms,
    ))
}

fn comparison_dimensions_match(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> bool {
    current.session_id != candidate.session_id
        && launch_modes_are_comparable(current, candidate)
        && required_version_targets_match(current, candidate)
        && current.scenario.requested_memory_mb == candidate.scenario.requested_memory_mb
        && required_dimensions_match(&current.device.tier, &candidate.device.tier)
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_profile.as_deref(),
            candidate.scenario.benchmark_profile.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_run_type.as_deref(),
            candidate.scenario.benchmark_run_type.as_deref(),
        )
        && optional_benchmark_dimensions_match(
            current.scenario.benchmark_mode.as_deref(),
            candidate.scenario.benchmark_mode.as_deref(),
        )
}

fn launch_modes_are_comparable(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> bool {
    matches!(
        (known_launch_mode(current), known_launch_mode(candidate)),
        (Some("managed"), Some("vanilla" | "managed"))
            | (Some("vanilla"), Some("vanilla"))
            | (Some("custom"), Some("custom"))
    )
}

fn known_launch_mode(report: &LaunchProofRecord) -> Option<&str> {
    let mode = normalized_dimension(&report.scenario.performance_mode)?;
    match mode {
        "managed" | "vanilla" | "custom"
            if required_dimensions_match(
                &report.scenario.scenario_id,
                scenario_id_for_performance_mode(mode),
            ) =>
        {
            Some(mode)
        }
        _ => None,
    }
}

fn required_dimensions_match(left: &str, right: &str) -> bool {
    match (normalized_dimension(left), normalized_dimension(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn optional_benchmark_dimensions_match(left: Option<&str>, right: Option<&str>) -> bool {
    match (
        left.and_then(normalized_dimension),
        right.and_then(normalized_dimension),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        _ => false,
    }
}

fn required_version_targets_match(
    current: &LaunchProofRecord,
    candidate: &LaunchProofRecord,
) -> bool {
    match (
        normalized_version_target(current),
        normalized_version_target(candidate),
    ) {
        (Some(current), Some(candidate)) => current == candidate,
        _ => false,
    }
}

fn normalized_dimension(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || value == "unknown" {
        None
    } else {
        Some(value)
    }
}

fn normalized_version_target(report: &LaunchProofRecord) -> Option<&str> {
    report
        .scenario
        .version_id
        .as_deref()
        .and_then(normalized_dimension)
        .or_else(|| normalized_dimension(&report.version_id))
}

// Metric source: launch stage history. The value is the sum of completed stage
// durations, using duration_ms when present and falling back to ended-started.
fn launch_total_completed_stage_duration_ms(report: &LaunchProofRecord) -> Option<u64> {
    let mut total = 0_u64;
    let mut completed = false;
    for stage in &report.stages {
        let Some(ended_at_ms) = stage.ended_at_ms else {
            continue;
        };
        let duration_ms = stage
            .duration_ms
            .unwrap_or_else(|| ended_at_ms.saturating_sub(stage.started_at_ms));
        total = total.saturating_add(duration_ms);
        completed = true;
    }
    completed.then_some(total)
}

fn launch_boot_duration_ms(report: &LaunchProofRecord) -> Option<u64> {
    report.boot_duration_ms
}

fn metric_delta_ms(current_value_ms: u64, baseline_value_ms: u64) -> i64 {
    if current_value_ms >= baseline_value_ms {
        i64::try_from(current_value_ms - baseline_value_ms).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(baseline_value_ms - current_value_ms).unwrap_or(i64::MAX)
    }
}

fn scenario_id_for_performance_mode(performance_mode: &str) -> &'static str {
    match performance_mode.trim() {
        "managed" => "managed_launch",
        "vanilla" => "vanilla_launch",
        "custom" => "custom_launch",
        _ => "unknown_launch",
    }
}

pub const MAX_CRASH_ARTIFACT_BYTES: usize = 512 * 1024;
pub const CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS: u64 = 15_000;
const MAX_LINES: usize = 4_096;
const MAX_LINE_BYTES: usize = 4_096;
const MAX_SUSPECTED_MODS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashArtifactKind {
    MinecraftCrashReport,
    JvmFatalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashFailurePhase {
    Startup,
    Initialization,
    Loading,
    Runtime,
    Shutdown,
    Native,
}

macro_rules! evidence_value {
    ($name:ident, $validator:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            fn checked(value: &str) -> Option<Self> {
                $validator(value).then(|| Self(value.to_string()))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::checked(&value)
                    .ok_or_else(|| de::Error::custom(concat!("invalid ", stringify!($name))))
            }
        }
    };
}

evidence_value!(CrashModName, is_safe_mod_name);
evidence_value!(CrashModVersion, is_safe_mod_version);
evidence_value!(CrashExceptionClass, is_throwable_class);
evidence_value!(CrashNativeModule, is_safe_native_module);
evidence_value!(CrashNativeSymbol, is_safe_native_identifier);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashNativeFrameKind {
    Native,
    Vm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashProblematicFrame {
    pub kind: CrashNativeFrameKind,
    pub module: CrashNativeModule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<CrashNativeSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashModEvidence {
    pub name: CrashModName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<CrashModVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrashEvidence {
    pub source: CrashArtifactKind,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_phase: Option<CrashFailurePhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<CrashExceptionClass>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspected_mods: Vec<CrashModEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problematic_frame: Option<CrashProblematicFrame>,
    pub names_out_of_memory: bool,
}

impl<'de> Deserialize<'de> for CrashEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            source: CrashArtifactKind,
            truncated: bool,
            failure_phase: Option<CrashFailurePhase>,
            exception_class: Option<CrashExceptionClass>,
            #[serde(default)]
            suspected_mods: Vec<CrashModEvidence>,
            problematic_frame: Option<CrashProblematicFrame>,
            names_out_of_memory: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.suspected_mods.len() > MAX_SUSPECTED_MODS {
            return Err(de::Error::custom("too many suspected mods"));
        }
        if wire
            .suspected_mods
            .iter()
            .enumerate()
            .any(|(index, candidate)| wire.suspected_mods[..index].contains(candidate))
        {
            return Err(de::Error::custom("duplicate suspected mod"));
        }
        let source_is_coherent = match wire.source {
            CrashArtifactKind::MinecraftCrashReport => {
                wire.problematic_frame.is_none()
                    && wire.failure_phase != Some(CrashFailurePhase::Native)
            }
            CrashArtifactKind::JvmFatalError => {
                wire.exception_class.is_none()
                    && wire.suspected_mods.is_empty()
                    && matches!(wire.failure_phase, None | Some(CrashFailurePhase::Native))
            }
        };
        if !source_is_coherent {
            return Err(de::Error::custom("incoherent crash evidence source"));
        }
        if wire.failure_phase.is_none()
            && wire.exception_class.is_none()
            && wire.suspected_mods.is_empty()
            && wire.problematic_frame.is_none()
            && !wire.names_out_of_memory
        {
            return Err(de::Error::custom("empty crash evidence"));
        }
        Ok(Self {
            source: wire.source,
            truncated: wire.truncated,
            failure_phase: wire.failure_phase,
            exception_class: wire.exception_class,
            suspected_mods: wire.suspected_mods,
            problematic_frame: wire.problematic_frame,
            names_out_of_memory: wire.names_out_of_memory,
        })
    }
}

#[derive(Debug)]
struct PendingMod {
    id: Option<String>,
    name: CrashModName,
    version: Option<CrashModVersion>,
}

struct CrashEvidenceBuilder {
    source: CrashArtifactKind,
    truncated: bool,
    failure_phase: Option<CrashFailurePhase>,
    exception_class: Option<CrashExceptionClass>,
    suspected_mods: Vec<PendingMod>,
    problematic_frame: Option<CrashProblematicFrame>,
    names_out_of_memory: bool,
    expect_problematic_frame: bool,
    expect_root_throwable: bool,
    current_failed_mod_id: Option<String>,
}

impl CrashEvidenceBuilder {
    fn new(source: CrashArtifactKind, truncated: bool) -> Self {
        Self {
            source,
            truncated,
            failure_phase: None,
            exception_class: None,
            suspected_mods: Vec::new(),
            problematic_frame: None,
            names_out_of_memory: false,
            expect_problematic_frame: false,
            expect_root_throwable: false,
            current_failed_mod_id: None,
        }
    }

    fn finish(self) -> Option<CrashEvidence> {
        let evidence = CrashEvidence {
            source: self.source,
            truncated: self.truncated,
            failure_phase: self.failure_phase,
            exception_class: self.exception_class,
            suspected_mods: self
                .suspected_mods
                .into_iter()
                .map(|entry| CrashModEvidence {
                    name: entry.name,
                    version: entry.version,
                })
                .collect(),
            problematic_frame: self.problematic_frame,
            names_out_of_memory: self.names_out_of_memory,
        };
        (evidence.failure_phase.is_some()
            || evidence.exception_class.is_some()
            || !evidence.suspected_mods.is_empty()
            || evidence.problematic_frame.is_some()
            || evidence.names_out_of_memory)
            .then_some(evidence)
    }

    fn inspect_line(&mut self, raw_line: &[u8]) {
        let line = String::from_utf8_lossy(raw_line);
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        self.names_out_of_memory |= is_out_of_memory_failure_line(line);
        if self.source == CrashArtifactKind::JvmFatalError {
            if self.expect_problematic_frame {
                self.expect_problematic_frame = false;
                if let Some(frame) = parse_problematic_frame(line) {
                    self.problematic_frame = Some(frame);
                    self.failure_phase.get_or_insert(CrashFailurePhase::Native);
                }
            }
            if line.eq_ignore_ascii_case("# Problematic frame:") {
                self.expect_problematic_frame = true;
            }
            return;
        }

        if let Some(section) = line
            .strip_prefix("-- ")
            .and_then(|value| value.strip_suffix(" --"))
        {
            self.current_failed_mod_id = section.strip_prefix("MOD ").and_then(sanitized_mod_id);
            return;
        }

        if let Some(phase) = parse_failure_phase(line) {
            self.failure_phase.get_or_insert(phase);
            self.expect_root_throwable = true;
            return;
        }
        if self.exception_class.is_none() {
            self.exception_class = parse_exception_class(line, self.expect_root_throwable);
        }
        self.expect_root_throwable = false;
        if let Some(value) = line.strip_prefix("Suspected Mods:") {
            self.add_suspected_mod_list(value);
        } else if let Some(value) = line.strip_prefix("Suspected Mod:") {
            self.add_suspected_mod(value);
        } else if let Some(value) = line.strip_prefix("Failure message:") {
            self.add_failed_section_mod(value);
        } else if let Some(value) = line.strip_prefix("Mod Version:") {
            self.enrich_current_version(value);
        } else if line.contains('|') {
            self.enrich_forge_mod_list(line);
        }
    }

    fn add_suspected_mod_list(&mut self, value: &str) {
        if value.trim().eq_ignore_ascii_case("none") {
            return;
        }
        for candidate in value.split(',') {
            self.add_suspected_mod(candidate);
            if self.suspected_mods.len() == MAX_SUSPECTED_MODS {
                break;
            }
        }
    }

    fn add_suspected_mod(&mut self, value: &str) {
        let value = value.trim();
        let (value, version) = value
            .rsplit_once(" version ")
            .map_or((value, None), |(name, version)| (name, Some(version)));
        let (name, id) = value
            .rsplit_once(" (")
            .and_then(|(name, id)| id.strip_suffix(')').map(|id| (name, id)))
            .map_or((value, value), |parts| parts);
        self.add_mod(id, name, version);
    }

    fn add_failed_section_mod(&mut self, value: &str) {
        let Some(id) = self.current_failed_mod_id.clone() else {
            return;
        };
        let (name, reported_id) = value
            .split_once(" (")
            .and_then(|(name, remainder)| {
                remainder
                    .split_once(')')
                    .map(|(reported_id, _)| (name.trim(), reported_id))
            })
            .filter(|(_, reported_id)| reported_id.eq_ignore_ascii_case(&id))
            .unwrap_or((&id, &id));
        self.add_mod(reported_id, name, None);
    }

    fn add_mod(&mut self, id: &str, name: &str, version: Option<&str>) {
        if self.suspected_mods.len() >= MAX_SUSPECTED_MODS {
            return;
        }
        let Some(name) = normalized_mod_name(name) else {
            return;
        };
        let id = sanitized_mod_id(id);
        let version = version.and_then(|value| CrashModVersion::checked(value.trim()));
        if self
            .suspected_mods
            .iter()
            .any(|entry| entry.id == id && entry.name == name)
        {
            return;
        }
        self.suspected_mods.push(PendingMod { id, name, version });
    }

    fn enrich_forge_mod_list(&mut self, line: &str) {
        let columns = line.split('|').map(str::trim).collect::<Vec<_>>();
        if columns.len() < 5 {
            return;
        }
        let (name, id, version) = (columns[1], columns[2], columns[3]);
        let Some(entry) = self.suspected_mods.iter_mut().find(|entry| {
            entry.id.as_deref() == Some(id) || entry.name.as_str().eq_ignore_ascii_case(name)
        }) else {
            return;
        };
        if entry.version.is_none() {
            entry.version = CrashModVersion::checked(version);
        }
    }

    fn enrich_current_version(&mut self, value: &str) {
        let Some(id) = self.current_failed_mod_id.as_deref() else {
            return;
        };
        let Some(entry) = self
            .suspected_mods
            .iter_mut()
            .find(|entry| entry.id.as_deref() == Some(id))
        else {
            return;
        };
        if entry.version.is_none() {
            entry.version = CrashModVersion::checked(value.trim());
        }
    }
}

pub fn parse_crash_evidence(source: CrashArtifactKind, raw: &[u8]) -> Option<CrashEvidence> {
    let bounded = &raw[..raw.len().min(MAX_CRASH_ARTIFACT_BYTES)];
    let mut lines = bounded.split(|byte| *byte == b'\n');
    let mut builder = CrashEvidenceBuilder::new(source, raw.len() > MAX_CRASH_ARTIFACT_BYTES);
    for _ in 0..MAX_LINES {
        let Some(line) = lines.next() else {
            return builder.finish();
        };
        if line.len() > MAX_LINE_BYTES {
            builder.truncated = true;
        }
        builder.inspect_line(&line[..line.len().min(MAX_LINE_BYTES)]);
    }
    builder.truncated |= lines.next().is_some();
    builder.finish()
}

pub(crate) fn is_out_of_memory_failure_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    let marker = "java.lang.outofmemoryerror";
    let throwable = lower
        .strip_prefix(marker)
        .is_some_and(has_throwable_boundary)
        || lower
            .strip_prefix("caused by: ")
            .and_then(|value| value.strip_prefix(marker))
            .is_some_and(has_throwable_boundary)
        || lower
            .strip_prefix("exception in thread ")
            .is_some_and(|value| {
                value.find(marker).is_some_and(|index| {
                    index > 0 && has_throwable_boundary(&value[index + marker.len()..])
                })
            });
    throwable
        || lower == "gc overhead limit exceeded"
        || lower == "# there is insufficient memory for the java runtime environment to continue."
        || lower
            .strip_prefix("# native memory allocation (")
            .and_then(|detail| detail.split_once(") failed to "))
            .is_some_and(|(_, failure)| {
                failure.starts_with("allocate ") || failure.starts_with("map ")
            })
        || lower
            .strip_prefix("# out of memory error (")
            .is_some_and(|detail| detail.ends_with(')'))
}

fn has_throwable_boundary(remainder: &str) -> bool {
    remainder
        .chars()
        .next()
        .is_none_or(|character| character == ':' || character.is_ascii_whitespace())
}

fn parse_failure_phase(line: &str) -> Option<CrashFailurePhase> {
    let description = line
        .strip_prefix("Description:")?
        .trim()
        .to_ascii_lowercase();
    if description.contains("initializ") {
        Some(CrashFailurePhase::Initialization)
    } else if description.contains("load") || description.contains("bootstrap") {
        Some(CrashFailurePhase::Loading)
    } else if description.contains("start") {
        Some(CrashFailurePhase::Startup)
    } else if description.contains("shut") || description.contains("stopp") {
        Some(CrashFailurePhase::Shutdown)
    } else if description.contains("tick")
        || description.contains("render")
        || description.contains("game")
    {
        Some(CrashFailurePhase::Runtime)
    } else {
        None
    }
}

fn parse_exception_class(line: &str, allow_bare: bool) -> Option<CrashExceptionClass> {
    let explicit = line
        .strip_prefix("Exception:")
        .or_else(|| line.strip_prefix("Caused by:"))
        .or_else(|| line.strip_prefix("Exception message:"));
    if explicit.is_none() && !allow_bare {
        return None;
    }
    let value = explicit.map(str::trim).unwrap_or(line);
    let (candidate, remainder) = value
        .split_once(':')
        .map_or((value, ""), |(candidate, remainder)| (candidate, remainder));
    if explicit.is_none() && remainder.is_empty() {
        return None;
    }
    CrashExceptionClass::checked(candidate.trim())
}

fn parse_problematic_frame(line: &str) -> Option<CrashProblematicFrame> {
    let value = line.strip_prefix('#').unwrap_or(line).trim();
    let (kind, value) = if let Some(value) = value
        .strip_prefix("C  ")
        .or_else(|| value.strip_prefix("C "))
    {
        (CrashNativeFrameKind::Native, value.trim())
    } else if let Some(value) = value
        .strip_prefix("V  ")
        .or_else(|| value.strip_prefix("V "))
    {
        (CrashNativeFrameKind::Vm, value.trim())
    } else {
        return None;
    };
    let start = value.find('[')?;
    let end = value[start..].find(']')? + start;
    let raw_frame = &value[start + 1..end];
    let (raw_module, raw_offset) = raw_frame.rsplit_once('+')?;
    if !is_native_offset(raw_offset) || raw_module.contains(['/', '\\']) {
        return None;
    }
    let module = CrashNativeModule::checked(strip_native_extension(raw_module))?;
    let symbol = value[end + 1..]
        .trim()
        .split('+')
        .next()
        .filter(|value| !value.is_empty())
        .and_then(CrashNativeSymbol::checked);
    Some(CrashProblematicFrame {
        kind,
        module,
        symbol,
    })
}

fn is_java_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || matches!(character, '_' | '$'))
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$'))
}

fn is_throwable_class(value: &str) -> bool {
    value.len() <= 128
        && value.split('.').count() >= 2
        && value.split('.').all(is_java_identifier)
        && value.rsplit('.').next().is_some_and(|name| {
            name.ends_with("Error") || name.ends_with("Exception") || name.ends_with("Throwable")
        })
}

fn is_safe_mod_name(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 96
        && value == value.trim()
        && !value.contains("  ")
        && !value.contains(['/', '\\', '@', '=', '[', ']'])
        && !lower.contains("bearer")
        && !lower.contains("token")
        && !lower.contains("username")
        && !lower.ends_with(".jar")
        && !lower.ends_with(".dll")
        && !lower.contains(".so")
        && !lower.ends_with(".dylib")
        && !looks_like_sensitive_public_value(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    ' ' | '.' | '_' | '-' | '+' | ':' | '#' | '(' | ')'
                )
        })
}

fn normalized_mod_name(value: &str) -> Option<CrashModName> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    CrashModName::checked(&normalized)
}

fn is_safe_mod_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value == value.trim()
        && !value.starts_with("-D")
        && !value.contains("..")
        && !looks_like_sensitive_public_value(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '+')
        })
}

fn is_safe_native_identifier(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 96
        && !lower.contains("token")
        && !lower.contains("bearer")
        && !looks_like_sensitive_public_value(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '$' | ':')
        })
}

fn looks_like_sensitive_public_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
        "account_id",
        "account-id",
        "username",
        "xuid",
        "bearer",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || looks_like_jwt(value)
        || (value.len() >= 48
            && !value.contains(' ')
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            }))
}

fn looks_like_jwt(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && value.len() >= 12
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
        })
}

fn is_safe_native_module(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    is_safe_native_identifier(value)
        && !lower.ends_with(".dll")
        && !lower.contains(".so")
        && !lower.ends_with(".dylib")
}

fn is_native_offset(value: &str) -> bool {
    value.strip_prefix("0x").is_some_and(|digits| {
        !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_hexdigit())
    })
}

fn strip_native_extension(value: &str) -> &str {
    if let Some((base, _)) = value.split_once(".so") {
        base
    } else if let Some(base) = value.strip_suffix(".dll") {
        base
    } else if let Some(base) = value.strip_suffix(".dylib") {
        base
    } else {
        value
    }
}

fn sanitized_mod_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 96
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        }))
    .then(|| value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported_report() -> LaunchProofRecord {
        let mut outcome = SessionOutcome {
            kind: SessionOutcomeKind::Clean,
            reason: super::super::outcome::SessionExitReason::CleanExit,
            failure_class: None,
            summary: String::new(),
        };
        outcome.summary = outcome.summary().to_owned();
        let mut report = LaunchProofRecord::from_session(SessionReportInput {
            session_id: format!("legacy-{}", "a".repeat(64)),
            instance_id: crate::instances::model::InstanceId::new().to_string(),
            version_id: "1.21.1".into(),
            launched_at: "2026-01-01T00:00:00.000Z".into(),
            ended_at: "2026-01-01T00:00:02.000Z".into(),
            outcome,
            entries: Vec::new(),
            exit_code: Some(0),
            boot_duration_ms: Some(1000),
            logs_dropped: 0,
        });
        report.scenario.performance_mode = "vanilla".into();
        report.scenario.scenario_id = "vanilla_launch".into();
        report.scenario.version_id = Some("1.21.1".into());
        report.device.tier = "mid".into();
        report.comparison = Some(LaunchProofComparison {
            baseline_session_id: format!("legacy-{}", "b".repeat(64)),
            baseline_recorded_at: "2025-12-31T00:00:00.000Z".into(),
            baseline: LaunchProofComparisonBaseline {
                performance_mode: "vanilla".into(),
                version_id: "1.21.1".into(),
                requested_memory_mb: None,
                device_tier: "mid".into(),
                benchmark_profile: None,
                benchmark_run_type: None,
                benchmark_mode: None,
            },
            matched_sample_count: 3,
            metric_name: "boot_duration_ms".into(),
            current_value_ms: 1000,
            baseline_value_ms: 2000,
            delta_ms: -1000,
            delta_percent: -50.0,
        });
        report
    }

    #[test]
    fn canonical_loader_versions_survive_reports_but_not_unstructured_logs() {
        use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
        for component in [
            LoaderComponentId::Fabric,
            LoaderComponentId::Quilt,
            LoaderComponentId::Forge,
            LoaderComponentId::NeoForge,
        ] {
            let version = installed_version_id_for(component, "1.20.1", "0.19.5").unwrap();
            let mut report = imported_report();
            report.version_id = version.clone();
            report.scenario.version_id = Some(version.clone());
            report.comparison.as_mut().unwrap().baseline.version_id = version.clone();
            report.logs.push(LogEntry {
                sequence: 1,
                source: super::super::logs::LogStream::Stdout,
                text: version.clone(),
                truncated: false,
            });
            let encoded = encode_report(&report).unwrap();
            let decoded = decode_report(&encoded).unwrap();
            assert_eq!(decoded.version_id, version);
            assert_eq!(
                decoded.scenario.version_id.as_deref(),
                Some(version.as_str())
            );
            assert_eq!(
                decoded.comparison.as_ref().unwrap().baseline.version_id,
                version
            );
            assert_eq!(decoded.logs[0].text, super::super::logs::REDACTED_LINE);
            assert_eq!(encode_report(&decoded).unwrap(), encoded);
            let metadata = Arc::new(MetadataStore::in_memory().unwrap());
            let store = LaunchReportStore::new(metadata).unwrap();
            store
                .record(decoded.clone(), &Redactor::new(vec![]))
                .unwrap();
            let saved = store.get(&decoded.session_id).unwrap().unwrap();
            assert_eq!(saved.version_id, version);
            assert_eq!(saved.scenario.version_id.as_deref(), Some(version.as_str()));
            let mut next = decoded;
            next.session_id = uuid::Uuid::new_v4().to_string();
            next.recorded_at = "2026-01-01T00:00:03.000Z".into();
            next.boot_duration_ms = Some(500);
            store.record(next.clone(), &Redactor::new(vec![])).unwrap();
            let saved = store.get(&next.session_id).unwrap().unwrap();
            assert_eq!(saved.comparison.unwrap().baseline.version_id, version);
        }
    }

    #[test]
    fn typed_loader_versions_reject_malformed_and_encoded_sensitive_values() {
        use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
        let canonical =
            installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.19.5").unwrap();
        let cases = [
            ("loader-v2-bad".to_owned(), vec![]),
            (format!("{canonical}="), vec![]),
            ("abc123".repeat(12), vec![]),
            (canonical.clone(), vec![canonical.clone()]),
            (canonical.clone(), vec![canonical[10..25].to_owned()]),
            (canonical.clone(), vec!["1.20.1".to_owned()]),
            (canonical.clone(), vec!["19.5".to_owned()]),
            (
                installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "raw-secret-token")
                    .unwrap(),
                vec![],
            ),
            (
                installed_version_id_for(
                    LoaderComponentId::Fabric,
                    "1.20.1",
                    "/private/credential",
                )
                .unwrap(),
                vec![],
            ),
        ];
        for (version, secrets) in cases {
            let redactor = Redactor::new(secrets);
            let mut report = imported_report();
            report.comparison = None;
            report.version_id = version.clone();
            report.scenario.version_id = Some(version.clone());
            sanitize_report(&mut report, &redactor).unwrap();
            assert_eq!(report.version_id, "unknown");
            assert!(report.scenario.version_id.is_none());
            let mut comparison = imported_report();
            comparison.comparison.as_mut().unwrap().baseline.version_id = version;
            assert!(sanitize_report(&mut comparison, &redactor).is_err());
        }
    }

    #[test]
    fn canonical_loader_report_uses_the_identity_codecs_length_bound() {
        let version = axial_minecraft::loaders::installed_version_id_for(
            axial_minecraft::loaders::LoaderComponentId::Fabric,
            "1.20.1",
            &"x".repeat(100),
        )
        .unwrap();
        assert!(version.len() > 96);
        let mut report = imported_report();
        report.version_id = version.clone();
        report.scenario.version_id = Some(version.clone());
        report.comparison.as_mut().unwrap().baseline.version_id = version.clone();
        let decoded = decode_report(&encode_report(&report).unwrap()).unwrap();
        assert_eq!(decoded.version_id, version);
        assert_eq!(
            decoded.scenario.version_id.as_deref(),
            Some(version.as_str())
        );
        assert_eq!(decoded.comparison.unwrap().baseline.version_id, version);
    }

    #[test]
    fn imported_report_reopen_is_immutable_including_its_historical_comparison() {
        let temporary_parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = tempfile::tempdir_in(temporary_parent).unwrap();
        let path = root.path().join("metadata.sqlite");
        let metadata = Arc::new(MetadataStore::open(&path).unwrap());
        let store = LaunchReportStore::new(metadata.clone()).unwrap();
        let report = imported_report();
        let prepared = PreparedReportImport::prepare(vec![report.clone()]).unwrap();
        metadata.transaction(|tx| prepared.insert_in(tx)).unwrap();
        assert_eq!(store.get(&report.session_id).unwrap(), Some(report.clone()));
        drop(store);
        drop(metadata);
        let metadata = Arc::new(MetadataStore::open(&path).unwrap());
        let store = LaunchReportStore::new(metadata.clone()).unwrap();
        metadata.transaction(|tx| prepared.insert_in(tx)).unwrap();
        metadata.transaction(|tx| prepared.verify_in(tx)).unwrap();
        assert_eq!(store.list_recent(25).unwrap(), vec![report.clone()]);
        let mut changed = report.clone();
        changed.comparison.as_mut().unwrap().matched_sample_count = 4;
        let changed = PreparedReportImport::prepare(vec![changed]).unwrap();
        assert!(matches!(
            metadata.transaction(|tx| changed.insert_in(tx)),
            Err(ReportError::ConflictingSession)
        ));
        assert_eq!(store.get(&report.session_id).unwrap(), Some(report));
    }

    #[test]
    fn imported_batch_rolls_back_and_index_corruption_is_not_an_identical_retry() {
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let store = LaunchReportStore::new(metadata.clone()).unwrap();
        let report = imported_report();
        let prepared = PreparedReportImport::prepare(vec![report.clone()]).unwrap();
        metadata.transaction(|tx| prepared.insert_in(tx)).unwrap();
        let mut new = report.clone();
        new.session_id = format!("legacy-{}", "c".repeat(64));
        let mut conflict = report.clone();
        conflict.exit_code = Some(1);
        let batch = PreparedReportImport::prepare(vec![new.clone(), conflict]).unwrap();
        assert!(matches!(
            metadata.transaction(|tx| batch.insert_in(tx)),
            Err(ReportError::ConflictingSession)
        ));
        assert!(store.get(&new.session_id).unwrap().is_none());
        metadata
            .transaction(|tx| -> Result<(), ReportError> {
                tx.execute(
                    "UPDATE launch_reports SET instance_id='different' WHERE session_id=?1",
                    [&report.session_id],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            metadata.transaction(|tx| prepared.verify_in(tx)),
            Err(ReportError::Invalid)
        ));
        assert!(matches!(
            metadata.transaction(|tx| prepared.insert_in(tx)),
            Err(ReportError::Invalid)
        ));
    }

    #[test]
    fn imported_preparation_rejects_noncanonical_lossy_and_duplicate_records() {
        let report = imported_report();
        assert!(PreparedReportImport::prepare(vec![report.clone(), report.clone()]).is_err());
        let mut ordinary = report.clone();
        ordinary.session_id = uuid::Uuid::new_v4().to_string();
        assert!(PreparedReportImport::prepare(vec![ordinary]).is_err());
        let mut noncanonical = report;
        noncanonical.session_outcome.summary = "Invented successful summary".into();
        assert!(PreparedReportImport::prepare(vec![noncanonical]).is_err());
    }

    const VANILLA: &[u8] = r###"---- Minecraft Crash Report ----
// This report includes local diagnostics that must not be exported.

Description: Rendering game

java.lang.OutOfMemoryError: Java heap space
	at com.mojang.blaze3d.systems.RenderSystem.replayQueue(RenderSystem.java:211)
	at net.minecraft.client.Minecraft.runTick(Minecraft.java:1234)

JVM Flags: -Duser.home=/home/alice -Dsession.token=access-token
Player: alice
"###
    .as_bytes();
    const FORGE: &[u8] = r###"---- Minecraft Crash Report ----

Description: Mod loading error has occurred

net.minecraftforge.fml.LoadingFailedException: Loading errors encountered

-- MOD examplemachines --
Details:
	Mod File: /home/alice/.minecraft/mods/example-machines-3.2.1.jar
	Failure message: Example Machines (examplemachines) encountered an error
	Mod Version: 3.2.1
	Exception message: java.lang.IllegalStateException: machine registry failed

-- MOD unrelatedtools --
Details:
	Mod File: /home/alice/.minecraft/mods/unrelated-tools-9.0.0.jar
	Mod Version: 9.0.0

Mod List:
	Forge-1.20.1.jar |Forge |forge |47.2.0 |COMMON_SET
	example-machines-3.2.1.jar |Example Machines |examplemachines |3.2.1 |ERROR
	unrelated-tools-9.0.0.jar |Unrelated Tools |unrelatedtools |9.0.0 |COMMON_SET

JVM Flags: -Dtoken=access-token
"###
    .as_bytes();
    const FABRIC: &[u8] = r###"---- Minecraft Crash Report ----

Description: Initializing game

Caused by: net.fabricmc.loader.api.EntrypointException: Exception while loading entries
	at net.fabricmc.loader.impl.FabricLoaderImpl.invokeEntrypoints(FabricLoaderImpl.java:384)

System Details:
	Fabric Mods: canvas: Canvas Renderer 1.4.2
	User: alice
	JVM Flags: -Duser.home=/home/alice
"###
    .as_bytes();
    const HS_ERR: &[u8] = r###"# A fatal error has been detected by the Java Runtime Environment:
#
#  SIGSEGV (0xb) at pc=0x00007f00, pid=1234, tid=1235
#
# Problematic frame:
# C  [libGLX_nvidia.so.0+0x5a13f]  glXSwapBuffers+0x2f
#
# Command Line: -Duser.home=/home/alice -Dsession.token=access-token
# User: alice
"###
    .as_bytes();
    const MALFORMED: &[u8] = r###"\0\0not a crash report
Description: /home/alice/Secret
Exception: ../../access-token
Suspected Mods: /home/alice/mod.jar (secret)
# Problematic frame:
# C  [/home/alice/libprivate.so+0x123]
"###
    .as_bytes();

    fn parse_report(raw: &[u8]) -> Option<CrashEvidence> {
        parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, raw)
    }

    #[test]
    fn parses_vanilla_exception_phase_and_exact_oom() {
        let evidence = parse_report(VANILLA).expect("vanilla evidence");
        assert_eq!(evidence.source, CrashArtifactKind::MinecraftCrashReport);
        assert_eq!(evidence.failure_phase, Some(CrashFailurePhase::Runtime));
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("java.lang.OutOfMemoryError")
        );
        assert!(evidence.names_out_of_memory);
        assert!(!evidence.truncated);
        assert!(evidence.suspected_mods.is_empty());
    }

    #[test]
    fn parses_only_failed_forge_mod_section_and_version() {
        let evidence = parse_report(FORGE).expect("forge evidence");
        assert_eq!(evidence.failure_phase, Some(CrashFailurePhase::Loading));
        assert_eq!(evidence.suspected_mods.len(), 1);
        assert_eq!(evidence.suspected_mods[0].name.as_str(), "Example Machines");
        assert_eq!(
            evidence.suspected_mods[0]
                .version
                .as_ref()
                .map(|value| value.as_str()),
            Some("3.2.1")
        );
    }

    #[test]
    fn fabric_inventory_is_not_treated_as_attribution() {
        let evidence = parse_report(FABRIC).expect("fabric evidence");
        assert_eq!(
            evidence.failure_phase,
            Some(CrashFailurePhase::Initialization)
        );
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("net.fabricmc.loader.api.EntrypointException")
        );
        assert!(evidence.suspected_mods.is_empty());
    }

    #[test]
    fn hs_err_exports_structured_module_and_symbol_without_extension_or_offset() {
        let evidence = parse_crash_evidence(CrashArtifactKind::JvmFatalError, HS_ERR)
            .expect("hs_err evidence");
        let frame = evidence.problematic_frame.expect("problematic frame");
        assert_eq!(frame.module.as_str(), "libGLX_nvidia");
        assert_eq!(
            frame.symbol.as_ref().map(|value| value.as_str()),
            Some("glXSwapBuffers")
        );
        let encoded = serde_json::to_string(&frame).unwrap();
        assert!(!encoded.contains(".so"));
        assert!(!encoded.contains("0x"));
    }

    #[test]
    fn hs_err_vm_frame_uses_the_same_structured_redaction() {
        let evidence = parse_crash_evidence(
            CrashArtifactKind::JvmFatalError,
            b"# Problematic frame:\n# V  [libjvm.so+0xc1a55] VMError::report_and_die+0x2",
        )
        .expect("VM frame");
        let frame = evidence.problematic_frame.expect("problematic frame");
        assert_eq!(frame.kind, CrashNativeFrameKind::Vm);
        assert_eq!(frame.module.as_str(), "libjvm");
        assert_eq!(
            frame.symbol.as_ref().map(|value| value.as_str()),
            Some("VMError::report_and_die")
        );
    }

    #[test]
    fn dotted_preamble_stack_and_oom_prose_do_not_become_evidence() {
        for raw in [
            "1.20.1 details\nexample.com support\nmod.example loaded",
            "at private.mod.MemoryException.run(MemoryException.java:42)",
            "Comment: Out of Memory Error is the title of this guide",
            "JVM Flags: -Dnote=java.lang.OutOfMemoryError -Duser.home=/home/alice",
        ] {
            assert!(parse_report(raw.as_bytes()).is_none(), "accepted {raw}");
        }
        let evidence =
            parse_report(b"Suspected Mods: Native memory allocation helper (memoryhelper)")
                .expect("safe suspected mod");
        assert!(!evidence.names_out_of_memory);

        let evidence = parse_report(
            b"com.attacker.FakeException: decoy\nDescription: Rendering game\njava.lang.IllegalStateException: real",
        )
        .expect("root throwable");
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("java.lang.IllegalStateException")
        );

        for helper in [
            "java.lang.OutOfMemoryErrorHelper: decoy",
            "Caused by: java.lang.OutOfMemoryErrorGuide: decoy",
            "Exception in thread main java.lang.OutOfMemoryErrorHelper: decoy",
        ] {
            assert!(!is_out_of_memory_failure_line(helper));
        }
    }

    #[test]
    fn artifact_kind_gates_extractors_and_wire_contract() {
        let spoofed_report = b"# Problematic frame:\n# C  [private.dll+0x12] secret+0x1";
        assert!(parse_report(spoofed_report).is_none());

        let spoofed_hs_err = b"Suspected Mods: Secret Mod (secretmod)\nDescription: Loading game";
        assert!(parse_crash_evidence(CrashArtifactKind::JvmFatalError, spoofed_hs_err).is_none());

        for incoherent in [
            r#"{"source":"minecraft_crash_report","truncated":false,"failure_phase":"native","exception_class":null,"suspected_mods":[],"problematic_frame":{"kind":"native","module":"nvoglv64","symbol":null},"names_out_of_memory":false}"#,
            r#"{"source":"jvm_fatal_error","truncated":false,"failure_phase":null,"exception_class":null,"suspected_mods":[{"name":"Example Mod"}],"problematic_frame":null,"names_out_of_memory":false}"#,
        ] {
            assert!(serde_json::from_str::<CrashEvidence>(incoherent).is_err());
        }
    }

    #[test]
    fn malformed_invalid_utf8_and_every_truncation_are_panic_free() {
        let mut malformed = MALFORMED.to_vec();
        malformed.extend_from_slice(&[0, 0xff, 0xfe]);
        let _ = parse_report(&malformed);
        for (kind, fixture) in [
            (CrashArtifactKind::MinecraftCrashReport, VANILLA),
            (CrashArtifactKind::MinecraftCrashReport, FORGE),
            (CrashArtifactKind::MinecraftCrashReport, FABRIC),
            (CrashArtifactKind::JvmFatalError, HS_ERR),
        ] {
            for length in 0..=fixture.len() {
                let _ = parse_crash_evidence(kind, &fixture[..length]);
            }
        }
    }

    #[test]
    fn truncation_is_explicit_and_work_is_bounded() {
        let mut before_cap =
            b"Description: Rendering game\njava.lang.IllegalStateException: first\n".to_vec();
        before_cap.resize(MAX_CRASH_ARTIFACT_BYTES + 32, b'x');
        let evidence = parse_report(&before_cap).expect("prefix evidence");
        assert!(evidence.truncated);

        let mut after_cap = vec![b'x'; MAX_CRASH_ARTIFACT_BYTES];
        after_cap.extend_from_slice(b"\njava.lang.IllegalStateException: hidden");
        assert!(parse_report(&after_cap).is_none());

        let huge_line = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(parse_report(&huge_line).is_none());
    }

    #[test]
    fn public_json_round_trip_revalidates_every_field() {
        for (kind, fixture) in [
            (CrashArtifactKind::MinecraftCrashReport, VANILLA),
            (CrashArtifactKind::MinecraftCrashReport, FORGE),
            (CrashArtifactKind::MinecraftCrashReport, FABRIC),
            (CrashArtifactKind::JvmFatalError, HS_ERR),
        ] {
            let evidence = parse_crash_evidence(kind, fixture).expect("fixture evidence");
            let encoded = serde_json::to_string(&evidence).expect("serialize");
            assert_eq!(
                serde_json::from_str::<CrashEvidence>(&encoded).unwrap(),
                evidence
            );
        }
    }

    #[test]
    fn public_json_rejects_empty_oversized_duplicate_and_sensitive_fields() {
        let base = |exception: &str, mods: &str, frame: &str| {
            format!(
                r#"{{"source":"minecraft_crash_report","truncated":false,"failure_phase":null,"exception_class":{exception},"suspected_mods":{mods},"problematic_frame":{frame},"names_out_of_memory":false}}"#
            )
        };
        for invalid in [
            base("null", "[]", "null"),
            base(r#""access-token""#, "[]", "null"),
            base("null", r#"[{"name":"Bearer raw-secret-token"}]"#, "null"),
            base("null", r#"[{"name":"alice@example.com"}]"#, "null"),
            base("null", r#"[{"name":"mod","version":"-Dtoken"}]"#, "null"),
            base("null", r#"[{"name":"SecretPlayer"}]"#, "null"),
            base("null", r#"[{"name":"account_id abc"}]"#, "null"),
            base("null", r#"[{"name":"Password credential"}]"#, "null"),
            base(
                "null",
                r#"[{"name":"mod","version":"access-token"}]"#,
                "null",
            ),
            base(
                "null",
                r#"[{"name":"mod","version":"abc.def.ghi123"}]"#,
                "null",
            ),
            base("null", r#"[{"name":"mod"},{"name":"mod"}]"#, "null"),
            base(
                "null",
                "[]",
                r#"{"kind":"native","module":"access-token","symbol":"secret"}"#,
            ),
            base(
                "null",
                "[]",
                r#"{"kind":"native","module":"raw_secret_value","symbol":null}"#,
            ),
        ] {
            assert!(serde_json::from_str::<CrashEvidence>(&invalid).is_err());
        }

        let long_name = "x".repeat(97);
        let invalid = base("null", &format!(r#"[{{"name":"{long_name}"}}]"#), "null");
        assert!(serde_json::from_str::<CrashEvidence>(&invalid).is_err());

        let long_version = "1".repeat(65);
        let invalid = base(
            "null",
            &format!(r#"[{{"name":"mod","version":"{long_version}"}}]"#),
            "null",
        );
        assert!(serde_json::from_str::<CrashEvidence>(&invalid).is_err());

        let long_exception = format!("example.{}Exception", "X".repeat(120));
        assert!(
            serde_json::from_str::<CrashEvidence>(&base(
                &format!(r#""{long_exception}""#),
                "[]",
                "null"
            ))
            .is_err()
        );

        let extension_frame = base(
            "null",
            "[]",
            r#"{"kind":"native","module":"private.dll","symbol":null}"#,
        );
        assert!(serde_json::from_str::<CrashEvidence>(&extension_frame).is_err());

        let mods = (0..=MAX_SUSPECTED_MODS)
            .map(|index| format!(r#"{{"name":"mod{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            serde_json::from_str::<CrashEvidence>(&base("null", &format!("[{mods}]"), "null"))
                .is_err()
        );
    }
}
