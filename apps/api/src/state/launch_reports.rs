use crate::logging::timestamp_utc;
use croopor_config::AppPaths;
use croopor_launcher::{
    GuardianSummary, LaunchHealingSummary, LaunchIntent, LaunchPriorityEvidence,
    LaunchSessionRecord, LaunchStageRecord, launch_state_name,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use sysinfo::System;

const LAUNCH_PROOF_SCHEMA: &str = "croopor.launch.proof";
const LAUNCH_PROOF_SCHEMA_VERSION: u32 = 1;
const LAUNCH_STAGE_COMPARISON_METRIC_NAME: &str = "total_completed_stage_duration_ms";
const LAUNCH_BOOT_COMPARISON_METRIC_NAME: &str = "boot_duration_ms";
const MAX_REPORT_FILENAME_STEM: usize = 96;
const MAX_BENCHMARK_METADATA_CHARS: usize = 96;
const MAX_EXPORT_TOKEN_CHARS: usize = 96;
const MAX_EXPORT_DETAIL_CHARS: usize = 180;
const MAX_EXPORT_DETAILS: usize = 8;
const MAX_EXPORT_STAGES: usize = 32;
// Conservative free-space warning threshold for launch caches, natives, and prewarm writes.
pub const LAUNCH_DISK_HEADROOM_MB: u64 = 2048;

type LaunchComparisonMetric = (&'static str, u64, fn(&LaunchProofRecord) -> Option<u64>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    pub scenario: LaunchProofScenario,
    pub device: LaunchProofDevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_budget: Option<LaunchProofResourceBudget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<LaunchProofPriority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardian: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healing: Option<Value>,
    pub stages: Vec<LaunchStageRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<LaunchProofComparison>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchProofExport {
    pub schema: String,
    pub schema_version: u32,
    pub session_id: String,
    pub instance_id: String,
    pub version_id: String,
    pub launched_at: String,
    pub recorded_at: String,
    pub outcome: String,
    pub scenario: LaunchProofScenario,
    pub device: LaunchProofDevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_budget: Option<LaunchProofResourceBudget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardian: Option<GuardianSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healing: Option<LaunchHealingSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<LaunchProofStageExport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<LaunchProofComparison>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaunchProofStageExport {
    pub stage: String,
    pub label: String,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
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
    pub matched_sample_count: usize,
    pub metric_name: String,
    pub current_value_ms: u64,
    pub baseline_value_ms: u64,
    pub delta_ms: i64,
    pub delta_percent: f64,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProofPriority {
    pub start_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion_error: Option<String>,
}

impl From<&LaunchPriorityEvidence> for LaunchProofPriority {
    fn from(value: &LaunchPriorityEvidence) -> Self {
        Self {
            start_mode: value.start_mode.clone(),
            start_error: value.start_error.clone(),
            promotion: value.promotion.clone(),
            promotion_error: value.promotion_error.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaunchBenchmarkMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl LaunchBenchmarkMetadata {
    pub fn new(
        benchmark_id: Option<&str>,
        profile: Option<&str>,
        run_type: Option<&str>,
        mode: Option<&str>,
    ) -> Self {
        Self {
            benchmark_id: benchmark_id.and_then(sanitize_benchmark_metadata),
            profile: profile.and_then(sanitize_benchmark_metadata),
            run_type: run_type.and_then(sanitize_benchmark_metadata),
            mode: mode.and_then(sanitize_benchmark_mode_metadata),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchProofContext {
    pub performance_mode: String,
    pub requested_memory_mb: Option<i32>,
    pub version_id: Option<String>,
    pub benchmark: Option<LaunchBenchmarkMetadata>,
    pub resource_budget: Option<LaunchProofResourceBudget>,
}

impl LaunchProofContext {
    pub fn from_intent(intent: &LaunchIntent) -> Self {
        Self {
            performance_mode: trimmed_or_unknown(&intent.performance_mode),
            requested_memory_mb: positive_i32(intent.max_memory_mb),
            version_id: non_empty_string(&intent.version_id),
            benchmark: None,
            resource_budget: None,
        }
    }

    pub fn with_benchmark(mut self, benchmark: Option<LaunchBenchmarkMetadata>) -> Self {
        self.benchmark = benchmark;
        self
    }

    pub fn with_resource_budget(
        mut self,
        resource_budget: Option<LaunchProofResourceBudget>,
    ) -> Self {
        self.resource_budget = resource_budget;
        self
    }
}

pub fn persist_record(
    paths: &AppPaths,
    record: &LaunchSessionRecord,
    launched_at: Option<&str>,
    outcome: &str,
) -> io::Result<LaunchProofRecord> {
    persist_record_with_context(paths, record, launched_at, outcome, None)
}

pub fn persist_record_with_context(
    paths: &AppPaths,
    record: &LaunchSessionRecord,
    launched_at: Option<&str>,
    outcome: &str,
    context: Option<&LaunchProofContext>,
) -> io::Result<LaunchProofRecord> {
    let mut proof = build_record(record, launched_at, outcome, context);
    proof.comparison = build_local_comparison(paths, &proof)?;
    let path = report_path(paths, &record.session_id.0);
    write_json_file(&path, &proof)?;
    Ok(proof)
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file(&temp_path, path)
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }
    if destination.exists() {
        let _ = fs::remove_file(destination);
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(source);
            Err(error)
        }
    }
}

pub fn list_recent(paths: &AppPaths, limit: usize) -> io::Result<Vec<LaunchProofRecord>> {
    let dir = report_dir(paths);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut reports = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .filter_map(|entry| load_file(&entry.path()).ok())
        .collect::<Vec<_>>();

    reports.sort_by(|left, right| {
        right
            .recorded_at
            .cmp(&left.recorded_at)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });
    reports.truncate(limit);
    Ok(reports)
}

pub fn list_recent_exports(paths: &AppPaths, limit: usize) -> io::Result<Vec<LaunchProofExport>> {
    list_recent(paths, limit)
        .map(|reports| reports.iter().map(LaunchProofExport::from_record).collect())
}

pub fn load(paths: &AppPaths, session_id: &str) -> io::Result<Option<LaunchProofRecord>> {
    let path = report_path(paths, session_id);
    match load_file(&path) {
        Ok(report) => Ok(Some(report)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn load_export(paths: &AppPaths, session_id: &str) -> io::Result<Option<LaunchProofExport>> {
    load(paths, session_id).map(|report| report.as_ref().map(LaunchProofExport::from_record))
}

pub fn report_path(paths: &AppPaths, session_id: &str) -> PathBuf {
    report_dir(paths).join(safe_report_filename(session_id))
}

impl LaunchProofExport {
    pub fn from_record(record: &LaunchProofRecord) -> Self {
        Self {
            schema: sanitized_required_token(&record.schema, LAUNCH_PROOF_SCHEMA),
            schema_version: record.schema_version,
            session_id: sanitized_required_token(&record.session_id, "redacted"),
            instance_id: sanitized_required_token(&record.instance_id, "redacted"),
            version_id: sanitized_required_token(&record.version_id, "unknown"),
            launched_at: sanitized_required_token(&record.launched_at, "unknown"),
            recorded_at: sanitized_required_token(&record.recorded_at, "unknown"),
            outcome: sanitized_required_token(&record.outcome, "unknown"),
            scenario: sanitized_export_scenario(&record.scenario),
            device: sanitized_export_device(&record.device),
            resource_budget: record.resource_budget.clone(),
            pid: record.pid,
            exit_code: record.exit_code,
            boot_duration_ms: record.boot_duration_ms,
            failure_class: record
                .failure_class
                .as_deref()
                .and_then(sanitized_optional_token),
            guardian: record.guardian.as_ref().and_then(sanitized_guardian),
            healing: record.healing.as_ref().and_then(sanitized_healing),
            stages: record
                .stages
                .iter()
                .take(MAX_EXPORT_STAGES)
                .map(sanitized_stage)
                .collect(),
            comparison: record.comparison.as_ref().map(sanitized_comparison),
        }
    }
}

fn sanitized_export_scenario(scenario: &LaunchProofScenario) -> LaunchProofScenario {
    LaunchProofScenario {
        scenario_id: sanitized_required_token(&scenario.scenario_id, "unknown_launch"),
        performance_mode: sanitized_required_token(&scenario.performance_mode, "unknown"),
        requested_memory_mb: scenario.requested_memory_mb,
        version_id: scenario
            .version_id
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_profile: scenario
            .benchmark_profile
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_run_type: scenario
            .benchmark_run_type
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_mode: scenario
            .benchmark_mode
            .as_deref()
            .and_then(sanitized_optional_token),
        benchmark_id: scenario
            .benchmark_id
            .as_deref()
            .and_then(sanitized_optional_token),
    }
}

fn sanitized_export_device(device: &LaunchProofDevice) -> LaunchProofDevice {
    LaunchProofDevice {
        tier: sanitized_required_token(&device.tier, "unknown"),
        total_memory_mb: device.total_memory_mb,
        cpu_threads: device.cpu_threads,
    }
}

fn sanitized_stage(stage: &LaunchStageRecord) -> LaunchProofStageExport {
    let stage_name = sanitized_required_token(&stage.stage, "unknown");
    LaunchProofStageExport {
        stage: stage_name.clone(),
        label: sanitized_bounded_text(&stage.label).unwrap_or_else(|| stage_name.clone()),
        started_at_ms: stage.started_at_ms,
        ended_at_ms: stage.ended_at_ms,
        duration_ms: stage.duration_ms,
        result: stage.result.as_deref().and_then(sanitized_optional_token),
        warnings: stage
            .warnings
            .iter()
            .filter_map(|warning| sanitized_bounded_text(warning))
            .take(MAX_EXPORT_DETAILS)
            .collect(),
        fallback_reason: stage
            .fallback_reason
            .as_deref()
            .and_then(sanitized_bounded_text),
    }
}

fn sanitized_guardian(value: &Value) -> Option<GuardianSummary> {
    let mut guardian = serde_json::from_value::<GuardianSummary>(value.clone()).ok()?;
    guardian.message = guardian.message.as_deref().and_then(sanitized_bounded_text);
    guardian.details = guardian
        .details
        .iter()
        .filter_map(|detail| sanitized_bounded_text(detail))
        .take(MAX_EXPORT_DETAILS)
        .collect();
    guardian.guidance = guardian
        .guidance
        .iter()
        .filter_map(|detail| sanitized_bounded_text(detail))
        .take(MAX_EXPORT_DETAILS)
        .collect();
    guardian.interventions = guardian
        .interventions
        .into_iter()
        .map(|mut intervention| {
            intervention.detail = intervention
                .detail
                .as_deref()
                .and_then(sanitized_bounded_text);
            intervention
        })
        .take(MAX_EXPORT_DETAILS)
        .collect();
    Some(guardian)
}

fn sanitized_healing(value: &Value) -> Option<LaunchHealingSummary> {
    let mut healing = serde_json::from_value::<LaunchHealingSummary>(value.clone()).ok()?;
    healing.requested_preset = healing
        .requested_preset
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.effective_preset = healing
        .effective_preset
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.auth_mode = healing
        .auth_mode
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.failure_class = healing
        .failure_class
        .as_deref()
        .and_then(sanitized_optional_token);
    healing.warnings = healing
        .warnings
        .iter()
        .filter_map(|warning| sanitized_bounded_text(warning))
        .take(MAX_EXPORT_DETAILS)
        .collect();
    healing.fallback_applied = healing
        .fallback_applied
        .as_deref()
        .and_then(sanitized_bounded_text);
    healing.events = healing
        .events
        .into_iter()
        .map(|mut event| {
            event.detail = event.detail.as_deref().and_then(sanitized_bounded_text);
            event
        })
        .take(MAX_EXPORT_DETAILS)
        .collect();
    Some(healing)
}

fn sanitized_comparison(comparison: &LaunchProofComparison) -> LaunchProofComparison {
    LaunchProofComparison {
        baseline_session_id: sanitized_required_token(&comparison.baseline_session_id, "redacted"),
        baseline_recorded_at: sanitized_required_token(&comparison.baseline_recorded_at, "unknown"),
        matched_sample_count: comparison.matched_sample_count,
        metric_name: sanitized_required_token(&comparison.metric_name, "unknown"),
        current_value_ms: comparison.current_value_ms,
        baseline_value_ms: comparison.baseline_value_ms,
        delta_ms: comparison.delta_ms,
        delta_percent: comparison.delta_percent,
    }
}

fn sanitized_required_token(value: &str, fallback: &str) -> String {
    sanitized_optional_token(value).unwrap_or_else(|| fallback.to_string())
}

fn sanitized_optional_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.chars().count() > MAX_EXPORT_TOKEN_CHARS
        || export_text_looks_sensitive(value)
        || !value.chars().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '+' | ':')
        })
    {
        return None;
    }

    Some(value.to_string())
}

fn sanitized_bounded_text(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.chars().count() > MAX_EXPORT_DETAIL_CHARS
        || export_text_looks_sensitive(&value)
    {
        return None;
    }

    Some(value)
}

fn export_text_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.contains('/') || value.contains('\\') {
        return true;
    }
    if lower.contains(".jar")
        || lower.contains(".exe")
        || lower.contains(".dll")
        || lower.contains(".dylib")
        || lower.contains(".so")
    {
        return true;
    }
    if lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.starts_with("-d")
        || lower.contains(" -d")
        || lower.contains("--access")
        || lower.contains("--username")
        || lower.contains("--uuid")
        || lower.contains("--xuid")
        || lower.contains("--user_properties")
        || lower.contains("--classpath")
        || lower.contains(" -cp ")
        || lower.contains(" -classpath ")
    {
        return true;
    }
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
        || lower.contains("account_id")
        || lower.contains("username=")
        || lower.contains("xuid=")
        || lower.contains("bearer ")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }
    if looks_like_jwt(value) || has_long_secret_like_run(value) {
        return true;
    }

    false
}

fn looks_like_jwt(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        let token = token.trim_matches(|value: char| {
            !(value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        });
        let parts = token.split('.').collect::<Vec<_>>();
        parts.len() >= 3
            && parts.iter().take(3).all(|part| {
                part.len() >= 12
                    && part
                        .chars()
                        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
            })
    })
}

fn has_long_secret_like_run(value: &str) -> bool {
    value
        .split(|value: char| !(value.is_ascii_alphanumeric() || matches!(value, '-' | '_')))
        .any(|part| {
            part.len() >= 48
                && part.chars().any(|value| value.is_ascii_alphabetic())
                && part.chars().any(|value| value.is_ascii_digit())
        })
}

fn build_record(
    record: &LaunchSessionRecord,
    launched_at: Option<&str>,
    outcome: &str,
    context: Option<&LaunchProofContext>,
) -> LaunchProofRecord {
    let recorded_at = timestamp_utc();
    let launched_at = launched_at
        .or(record.launched_at.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(recorded_at.as_str())
        .to_string();
    let outcome = if outcome.trim().is_empty() {
        launch_state_name(record.state).to_string()
    } else {
        outcome.to_string()
    };

    LaunchProofRecord {
        schema: LAUNCH_PROOF_SCHEMA.to_string(),
        schema_version: LAUNCH_PROOF_SCHEMA_VERSION,
        session_id: record.session_id.0.clone(),
        instance_id: record.instance_id.clone(),
        version_id: record.version_id.clone(),
        launched_at,
        recorded_at,
        outcome,
        scenario: build_scenario(record, context),
        device: local_device_metadata(),
        resource_budget: context.and_then(|value| value.resource_budget.clone()),
        pid: record.pid,
        exit_code: record.exit_code,
        boot_duration_ms: record.boot_duration_ms,
        priority: record.priority.as_ref().map(LaunchProofPriority::from),
        failure_class: record
            .failure
            .as_ref()
            .map(|failure| failure.class.as_str().to_string()),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.clone()),
        guardian: record.guardian.clone(),
        healing: record.healing.clone(),
        stages: record.stages.clone(),
        comparison: None,
    }
}

fn build_local_comparison(
    paths: &AppPaths,
    current: &LaunchProofRecord,
) -> io::Result<Option<LaunchProofComparison>> {
    let dir = report_dir(paths);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut candidates = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .filter_map(|entry| load_file(&entry.path()).ok())
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .recorded_at
            .cmp(&left.recorded_at)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });

    Ok(build_comparison_from_candidates(current, &candidates))
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
        matched_sample_count,
        metric_name: metric_name.to_string(),
        current_value_ms,
        baseline_value_ms: *baseline_value_ms,
        delta_ms,
        delta_percent: (delta_ms as f64 / *baseline_value_ms as f64) * 100.0,
    })
}

fn comparison_baseline_mode_rank(current: &LaunchProofRecord, candidate: &LaunchProofRecord) -> u8 {
    match (known_launch_mode(current), known_launch_mode(candidate)) {
        (Some("managed"), Some("vanilla")) => 0,
        _ => 1,
    }
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

fn build_scenario(
    record: &LaunchSessionRecord,
    context: Option<&LaunchProofContext>,
) -> LaunchProofScenario {
    let performance_mode = context
        .map(|value| trimmed_or_unknown(&value.performance_mode))
        .unwrap_or_else(|| "unknown".to_string());
    let version_id = context
        .and_then(|value| value.version_id.clone())
        .or_else(|| non_empty_string(&record.version_id));
    let benchmark = context.and_then(|value| value.benchmark.as_ref());

    LaunchProofScenario {
        scenario_id: scenario_id_for_performance_mode(&performance_mode).to_string(),
        performance_mode,
        requested_memory_mb: context.and_then(|value| value.requested_memory_mb),
        version_id,
        benchmark_profile: benchmark.and_then(|value| value.profile.clone()),
        benchmark_run_type: benchmark.and_then(|value| value.run_type.clone()),
        benchmark_mode: benchmark.and_then(|value| value.mode.clone()),
        benchmark_id: benchmark.and_then(|value| value.benchmark_id.clone()),
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

fn local_device_metadata() -> LaunchProofDevice {
    let total_memory_mb = host_total_memory_mb();
    let cpu_threads = std::thread::available_parallelism().ok().map(usize::from);

    LaunchProofDevice {
        tier: classify_device_tier(cpu_threads, total_memory_mb).to_string(),
        total_memory_mb,
        cpu_threads,
    }
}

fn host_total_memory_mb() -> Option<u64> {
    let mut system = System::new();
    system.refresh_memory();
    let total_memory_mb = system.total_memory() / (1024 * 1024);
    (total_memory_mb > 0).then_some(total_memory_mb)
}

fn classify_device_tier(cpu_threads: Option<usize>, total_memory_mb: Option<u64>) -> &'static str {
    let mut tiers = Vec::new();
    if let Some(cpu_threads) = cpu_threads.filter(|value| *value > 0) {
        tiers.push(if cpu_threads <= 4 {
            DeviceTier::Low
        } else if cpu_threads >= 8 {
            DeviceTier::High
        } else {
            DeviceTier::Mid
        });
    }
    if let Some(total_memory_mb) = total_memory_mb.filter(|value| *value > 0) {
        tiers.push(if total_memory_mb <= 8_192 {
            DeviceTier::Low
        } else if total_memory_mb >= 32_768 {
            DeviceTier::High
        } else {
            DeviceTier::Mid
        });
    }

    match tiers.into_iter().min() {
        Some(DeviceTier::Low) => "low",
        Some(DeviceTier::Mid) => "mid",
        Some(DeviceTier::High) => "high",
        None => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DeviceTier {
    Low,
    Mid,
    High,
}

fn trimmed_or_unknown(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.to_string()
    }
}

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn sanitize_benchmark_metadata(value: &str) -> Option<String> {
    let sanitized = value
        .trim()
        .chars()
        .filter(|value| !value.is_control())
        .take(MAX_BENCHMARK_METADATA_CHARS)
        .collect::<String>();
    non_empty_string(&sanitized)
}

fn sanitize_benchmark_mode_metadata(value: &str) -> Option<String> {
    let sanitized = sanitize_benchmark_metadata(value)?;
    let normalized = match sanitized.as_str() {
        "development" => "development",
        "qualification" => "qualification",
        "release_validation" => "release_validation",
        _ => return None,
    };
    Some(normalized.to_string())
}

fn positive_i32(value: i32) -> Option<i32> {
    (value > 0).then_some(value)
}

fn load_file(path: &Path) -> io::Result<LaunchProofRecord> {
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn report_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("benchmarks").join("launch")
}

fn safe_report_filename(session_id: &str) -> String {
    let mut stem = session_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    stem.truncate(MAX_REPORT_FILENAME_STEM);
    let stem = stem.trim_matches('_');
    if stem.is_empty() {
        "session.json".to_string()
    } else {
        format!("{stem}.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use croopor_config::AppPaths;
    use croopor_launcher::service::HealingSummaryInput;
    use croopor_launcher::{
        LaunchFailure, LaunchFailureClass, LaunchState, SessionId, build_healing_summary,
        launch_stage_label,
    };
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launch_report_path_sanitizes_session_id_to_config_subdirectory() {
        let root = test_root("safe-path");
        let paths = test_paths(&root);

        let path = report_path(&paths, "../bad/session\\id:?");

        assert_eq!(path.parent(), Some(report_dir(&paths).as_path()));
        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("bad_session_id.json")
        );
        assert!(path.starts_with(paths.config_dir.join("benchmarks").join("launch")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_json_and_lists_recent_records() {
        let root = test_root("persist-list");
        let paths = test_paths(&root);
        let mut first = test_record("first");
        first.pid = Some(11);
        first.failure = Some(LaunchFailure {
            class: LaunchFailureClass::StartupStalled,
            detail: Some("no startup activity observed".to_string()),
        });

        let first_proof =
            persist_record(&paths, &first, Some("2026-01-02T03:04:05.000Z"), "failed")
                .expect("persist first report");
        let second = persist_record(&paths, &test_record("second"), None, "running")
            .expect("persist second report");

        assert!(
            !report_path(&paths, "first")
                .with_extension("json.tmp")
                .exists()
        );
        assert!(
            !report_path(&paths, "second")
                .with_extension("json.tmp")
                .exists()
        );
        assert_eq!(first_proof.schema, LAUNCH_PROOF_SCHEMA);
        assert_eq!(first_proof.schema_version, LAUNCH_PROOF_SCHEMA_VERSION);
        assert_eq!(
            first_proof.failure_class.as_deref(),
            Some("startup_stalled")
        );
        assert_eq!(first_proof.pid, Some(11));
        assert_eq!(first_proof.boot_duration_ms, None);
        assert_eq!(first_proof.launched_at, "2026-01-02T03:04:05.000Z");
        assert_eq!(first_proof.guardian, Some(json!({ "mode": "managed" })));
        assert_eq!(first_proof.scenario.scenario_id, "unknown_launch");
        assert_eq!(first_proof.scenario.performance_mode, "unknown");
        assert_eq!(first_proof.scenario.version_id.as_deref(), Some("1.21.1"));
        assert_eq!(first_proof.scenario.benchmark_profile, None);
        assert_eq!(first_proof.scenario.benchmark_run_type, None);
        assert_eq!(first_proof.scenario.benchmark_mode, None);
        assert_eq!(first_proof.scenario.benchmark_id, None);
        assert_eq!(first_proof.comparison, None);
        assert!(matches!(
            first_proof.device.tier.as_str(),
            "low" | "mid" | "high" | "unknown"
        ));
        assert!(
            first_proof
                .stages
                .iter()
                .any(|stage| stage.stage == "queued")
        );
        let persisted_json =
            fs::read_to_string(report_path(&paths, "first")).expect("read persisted report");
        assert!(!persisted_json.contains("command"));
        assert!(!persisted_json.contains("boot_duration_ms"));
        assert!(!persisted_json.contains("-Xmx2048M"));
        assert!(!persisted_json.contains("java_path"));

        let loaded = load(&paths, "first")
            .expect("load report")
            .expect("report exists");
        assert_eq!(loaded.session_id, "first");
        assert_eq!(loaded.outcome, "failed");

        let recent = list_recent(&paths, 10).expect("list reports");
        assert_eq!(recent.len(), 2);
        assert!(
            recent
                .iter()
                .any(|report| report.session_id == second.session_id)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_without_current_structural_fields_is_invalid() {
        let root = test_root("missing-current-fields");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "missing-current-fields"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "missing-current-fields",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "exited"
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let error = load(&paths, "missing-current-fields")
            .expect_err("missing scenario/device/stages should be invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_boot_duration_when_session_record_has_marker_timing() {
        let root = test_root("boot-duration");
        let paths = test_paths(&root);
        let mut record = test_record("boot-duration");
        record.process_started_at_ms = Some(1_000);
        record.boot_completed_at_ms = Some(5_250);
        record.boot_duration_ms = Some(4_250);

        let proof = persist_record(&paths, &record, None, "running")
            .expect("persist report with boot duration");

        assert_eq!(proof.boot_duration_ms, Some(4_250));
        let persisted_json =
            fs::read_to_string(report_path(&paths, "boot-duration")).expect("read report");
        assert!(persisted_json.contains("\"boot_duration_ms\": 4250"));
        assert!(!persisted_json.contains("process_started_at_ms"));
        assert!(!persisted_json.contains("boot_completed_at_ms"));

        let loaded = load(&paths, "boot-duration")
            .expect("load report")
            .expect("report exists");
        assert_eq!(loaded.boot_duration_ms, Some(4_250));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_priority_evidence_without_empty_error_fields() {
        let root = test_root("priority-evidence");
        let paths = test_paths(&root);
        let mut record = test_record("priority-evidence");
        record.priority = Some(LaunchPriorityEvidence {
            start_mode: "below_normal_until_boot".to_string(),
            start_error: None,
            promotion: Some("promoted".to_string()),
            promotion_error: None,
        });

        let proof =
            persist_record(&paths, &record, None, "running").expect("persist priority evidence");

        assert_eq!(
            proof.priority,
            Some(LaunchProofPriority {
                start_mode: "below_normal_until_boot".to_string(),
                start_error: None,
                promotion: Some("promoted".to_string()),
                promotion_error: None,
            })
        );
        let persisted_json =
            fs::read_to_string(report_path(&paths, "priority-evidence")).expect("read report");
        assert!(persisted_json.contains("\"priority\""));
        assert!(persisted_json.contains("\"start_mode\": \"below_normal_until_boot\""));
        assert!(persisted_json.contains("\"promotion\": \"promoted\""));
        assert!(!persisted_json.contains("start_error"));
        assert!(!persisted_json.contains("promotion_error"));
        assert!(!persisted_json.contains("process_started_at_ms"));
        assert!(!persisted_json.contains("boot_completed_at_ms"));

        let loaded = load(&paths, "priority-evidence")
            .expect("load report")
            .expect("report exists");
        assert_eq!(loaded.priority, proof.priority);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_without_optional_benchmark_metadata_loads() {
        let root = test_root("no-benchmark");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "benchmark-free"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "benchmark-free",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "running",
                "scenario": {
                    "scenario_id": "managed_launch",
                    "performance_mode": "managed",
                    "requested_memory_mb": 4096,
                    "version_id": "1.21.1"
                },
                "device": {
                    "tier": "mid",
                    "total_memory_mb": 16384,
                    "cpu_threads": 6
                },
                "stages": []
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let loaded = load(&paths, "benchmark-free")
            .expect("load report")
            .expect("report exists");

        assert_eq!(loaded.scenario.scenario_id, "managed_launch");
        assert_eq!(loaded.scenario.benchmark_profile, None);
        assert_eq!(loaded.scenario.benchmark_run_type, None);
        assert_eq!(loaded.scenario.benchmark_mode, None);
        assert_eq!(loaded.scenario.benchmark_id, None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_priority_with_unknown_fields_is_invalid() {
        let root = test_root("priority-unknown-field");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "priority-unknown-field"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "priority-unknown-field",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "running",
                "scenario": {
                    "scenario_id": "managed_launch",
                    "performance_mode": "managed",
                    "requested_memory_mb": 4096,
                    "version_id": "1.21.1"
                },
                "device": {
                    "tier": "mid",
                    "total_memory_mb": 16384,
                    "cpu_threads": 6
                },
                "priority": {
                    "start_mode": "noop",
                    "unexpected": true
                },
                "stages": []
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let error = load(&paths, "priority-unknown-field")
            .expect_err("unknown priority field should be invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn matching_previous_launch_report_produces_comparison() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("matching report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.baseline_recorded_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.metric_name, LAUNCH_STAGE_COMPARISON_METRIC_NAME);
        assert_eq!(comparison.current_value_ms, 90);
        assert_eq!(comparison.baseline_value_ms, 120);
        assert_eq!(comparison.delta_ms, -30);
        assert_eq!(comparison.delta_percent, -25.0);
    }

    #[test]
    fn normal_launch_reports_without_benchmark_metadata_still_compare() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("normal launch report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
    }

    #[test]
    fn empty_or_unknown_benchmark_metadata_is_compatible_with_normal_launch_reports() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        previous.scenario.benchmark_profile = Some(" ".to_string());
        previous.scenario.benchmark_run_type = Some("unknown".to_string());
        previous.scenario.benchmark_mode = Some("unknown".to_string());

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("normal launch report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
    }

    #[test]
    fn benchmark_launch_reports_with_matching_metadata_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(
            &mut previous,
            "development-default",
            "repeat",
            "development",
        );

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("matching benchmark report comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
    }

    #[test]
    fn managed_benchmark_launch_report_compares_to_matching_vanilla_baseline() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut vanilla, "vanilla");
        set_benchmark_metadata(&mut vanilla, "development-default", "repeat", "development");

        let comparison = build_comparison_from_candidates(&current, &[vanilla])
            .expect("matching vanilla benchmark report comparison");

        assert_eq!(comparison.baseline_session_id, "vanilla");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.baseline_value_ms, 120);
    }

    #[test]
    fn managed_benchmark_launch_report_prefers_vanilla_over_newer_managed_baseline() {
        let mut current = comparison_report("current", "2026-01-03T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut vanilla, "vanilla");
        set_benchmark_metadata(&mut vanilla, "development-default", "repeat", "development");
        let mut managed = comparison_report("managed", "2026-01-02T00:00:00.000Z", 110);
        set_benchmark_metadata(&mut managed, "development-default", "repeat", "development");

        let comparison = build_comparison_from_candidates(&current, &[vanilla, managed])
            .expect("matching benchmark report comparison");

        assert_eq!(comparison.baseline_session_id, "vanilla");
        assert_eq!(comparison.matched_sample_count, 2);
        assert_eq!(comparison.baseline_value_ms, 120);
    }

    #[test]
    fn vanilla_launch_report_does_not_compare_to_managed_launch_report() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_launch_mode(&mut current, "vanilla");
        let managed = comparison_report("managed", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[managed]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn custom_launch_report_does_not_compare_to_managed_or_vanilla_launch_report() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_launch_mode(&mut current, "custom");
        let managed = comparison_report("managed", "2026-01-01T00:00:00.000Z", 120);
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:01.000Z", 130);
        set_launch_mode(&mut vanilla, "vanilla");

        let comparison = build_comparison_from_candidates(&current, &[managed, vanilla]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn unknown_or_empty_launch_modes_do_not_cross_compare() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut unknown = comparison_report("unknown", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut unknown, "unknown");
        let mut empty = comparison_report("empty", "2026-01-01T00:00:01.000Z", 130);
        empty.scenario.scenario_id.clear();
        empty.scenario.performance_mode.clear();

        let comparison = build_comparison_from_candidates(&current, &[unknown, empty]);

        assert_eq!(comparison, None);

        let mut current_unknown =
            comparison_report("current-unknown", "2026-01-02T00:00:00.000Z", 90);
        set_launch_mode(&mut current_unknown, "unknown");
        let mut vanilla = comparison_report("vanilla", "2026-01-01T00:00:00.000Z", 120);
        set_launch_mode(&mut vanilla, "vanilla");

        let comparison = build_comparison_from_candidates(&current_unknown, &[vanilla]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_reports_with_different_profile_do_not_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(&mut previous, "release-default", "repeat", "development");

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_reports_with_different_run_type_do_not_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "cold", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(
            &mut previous,
            "development-default",
            "repeat",
            "development",
        );

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_reports_with_different_mode_do_not_compare() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        set_benchmark_metadata(
            &mut previous,
            "development-default",
            "repeat",
            "release_validation",
        );

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn benchmark_launch_report_does_not_compare_to_normal_launch_report() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        set_benchmark_metadata(&mut current, "development-default", "repeat", "development");
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn matching_boot_duration_launch_report_uses_boot_duration_comparison() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.boot_duration_ms = Some(50);
        let mut previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);
        previous.boot_duration_ms = Some(75);

        let comparison = build_comparison_from_candidates(&current, &[previous])
            .expect("matching boot duration comparison");

        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.metric_name, LAUNCH_BOOT_COMPARISON_METRIC_NAME);
        assert_eq!(comparison.current_value_ms, 50);
        assert_eq!(comparison.baseline_value_ms, 75);
        assert_eq!(comparison.delta_ms, -25);
    }

    #[test]
    fn current_boot_duration_launch_report_does_not_compare_to_stage_only_candidate() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.boot_duration_ms = Some(50);
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn failed_current_launch_report_does_not_compare_to_matching_successful_candidate() {
        let mut current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        current.outcome = "failed".to_string();
        let previous = comparison_report("baseline", "2026-01-01T00:00:00.000Z", 120);

        let comparison = build_comparison_from_candidates(&current, &[previous]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn failed_candidate_is_ignored_for_launch_report_comparison() {
        let current = comparison_report("current", "2026-01-03T00:00:00.000Z", 90);
        let successful = comparison_report("successful", "2026-01-01T00:00:00.000Z", 120);
        let mut failed = comparison_report("failed", "2026-01-02T00:00:00.000Z", 10);
        failed.outcome = "failed".to_string();

        let comparison = build_comparison_from_candidates(&current, &[successful, failed])
            .expect("matching successful report comparison");

        assert_eq!(comparison.baseline_session_id, "successful");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.baseline_value_ms, 120);
        assert_eq!(comparison.delta_ms, -30);
    }

    #[test]
    fn failed_candidate_does_not_produce_launch_report_comparison() {
        let current = comparison_report("current", "2026-01-03T00:00:00.000Z", 90);
        let mut failed = comparison_report("failed", "2026-01-02T00:00:00.000Z", 10);
        failed.outcome = "failed".to_string();

        let comparison = build_comparison_from_candidates(&current, &[failed]);

        assert_eq!(comparison, None);
    }

    #[test]
    fn nonmatching_version_device_or_memory_does_not_compare() {
        let current = comparison_report("current", "2026-01-02T00:00:00.000Z", 90);
        let mut wrong_version = comparison_report("wrong-version", "2026-01-01T00:00:00.000Z", 120);
        wrong_version.scenario.version_id = Some("1.20.6".to_string());
        wrong_version.version_id = "1.20.6".to_string();
        let mut wrong_device = comparison_report("wrong-device", "2026-01-01T00:00:01.000Z", 120);
        wrong_device.device.tier = "high".to_string();
        let mut wrong_memory = comparison_report("wrong-memory", "2026-01-01T00:00:02.000Z", 120);
        wrong_memory.scenario.requested_memory_mb = Some(8192);

        let comparison = build_comparison_from_candidates(
            &current,
            &[wrong_version, wrong_device, wrong_memory],
        );

        assert_eq!(comparison, None);
    }

    #[test]
    fn persisted_launch_report_compares_to_previous_matching_local_report() {
        let root = test_root("persist-comparison");
        let paths = test_paths(&root);
        let context = LaunchProofContext {
            performance_mode: "managed".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: None,
            resource_budget: None,
        };
        let baseline = test_record_with_stage_duration("baseline", 120);
        let current = test_record_with_stage_duration("current", 90);

        let first = persist_record_with_context(&paths, &baseline, None, "exited", Some(&context))
            .expect("persist baseline report");
        let second = persist_record_with_context(&paths, &current, None, "exited", Some(&context))
            .expect("persist current report");

        assert_eq!(first.comparison, None);
        let comparison = second.comparison.expect("persisted comparison");
        assert_eq!(comparison.baseline_session_id, "baseline");
        assert_eq!(comparison.matched_sample_count, 1);
        assert_eq!(comparison.metric_name, LAUNCH_STAGE_COMPARISON_METRIC_NAME);
        assert_eq!(comparison.current_value_ms, 90);
        assert_eq!(comparison.baseline_value_ms, 120);
        assert_eq!(comparison.delta_ms, -30);

        let persisted_json =
            fs::read_to_string(report_path(&paths, "current")).expect("read current report");
        assert!(persisted_json.contains("\"comparison\""));
        assert!(!persisted_json.contains("-Xmx2048M"));
        assert!(!persisted_json.contains("/usr/bin/java"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_builds_benchmark_scenario_from_context_without_sensitive_fields() {
        let root = test_root("scenario-context");
        let paths = test_paths(&root);
        let record = test_record("scenario");
        let context = LaunchProofContext {
            performance_mode: " managed ".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: Some(LaunchBenchmarkMetadata::new(
                Some(" benchmark-1 "),
                Some(" dev-default\n"),
                Some(" repeat "),
                Some("release_validation"),
            )),
            resource_budget: None,
        };

        let proof = persist_record_with_context(
            &paths,
            &record,
            Some("2026-01-02T03:04:05.000Z"),
            "running",
            Some(&context),
        )
        .expect("persist report");

        assert_eq!(
            proof.scenario,
            LaunchProofScenario {
                scenario_id: "managed_launch".to_string(),
                performance_mode: "managed".to_string(),
                requested_memory_mb: Some(4096),
                version_id: Some("1.21.4".to_string()),
                benchmark_profile: Some("dev-default".to_string()),
                benchmark_run_type: Some("repeat".to_string()),
                benchmark_mode: Some("release_validation".to_string()),
                benchmark_id: Some("benchmark-1".to_string()),
            }
        );

        let persisted_json =
            fs::read_to_string(report_path(&paths, "scenario")).expect("read persisted report");
        assert!(persisted_json.contains("\"scenario\""));
        assert!(persisted_json.contains("\"device\""));
        assert!(persisted_json.contains("\"benchmark_profile\": \"dev-default\""));
        assert!(persisted_json.contains("\"benchmark_run_type\": \"repeat\""));
        assert!(persisted_json.contains("\"benchmark_mode\": \"release_validation\""));
        assert!(persisted_json.contains("\"benchmark_id\": \"benchmark-1\""));
        assert!(!persisted_json.contains("command"));
        assert!(!persisted_json.contains("java_path"));
        assert!(!persisted_json.contains("/usr/bin/java"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_resource_budget_without_sensitive_fields() {
        let root = test_root("resource-budget-context");
        let paths = test_paths(&root);
        let mut record = test_record("resource-budget");
        record.command.push("-Dauth.username=Player".to_string());
        let resource_budget = LaunchProofResourceBudget {
            host_total_memory_mb: Some(8192),
            host_available_memory_mb: Some(4096),
            host_used_memory_mb: Some(4096),
            host_cpu_threads: Some(4),
            host_cpu_load_1m_x100: Some(42),
            host_cpu_load_5m_x100: Some(35),
            host_cpu_load_15m_x100: Some(21),
            launcher_process_memory_mb: Some(128),
            active_session_count: 1,
            active_install_count: 1,
            active_memory_allocation_mb: 3072,
            requested_memory_mb: Some(4096),
            estimated_remaining_memory_mb: Some(1024),
            memory_headroom_mb: 2048,
            memory_pressure: true,
            cpu_pressure: true,
            install_pressure: true,
            launch_disk_available_mb: Some(1536),
            launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
            disk_pressure: true,
        };
        let context = LaunchProofContext {
            performance_mode: "managed".to_string(),
            requested_memory_mb: Some(4096),
            version_id: Some("1.21.4".to_string()),
            benchmark: None,
            resource_budget: Some(resource_budget.clone()),
        };

        let proof = persist_record_with_context(&paths, &record, None, "running", Some(&context))
            .expect("persist report");

        assert_eq!(proof.resource_budget, Some(resource_budget));
        let persisted_json = fs::read_to_string(report_path(&paths, "resource-budget"))
            .expect("read persisted report");
        assert!(persisted_json.contains("\"resource_budget\""));
        assert!(persisted_json.contains("\"host_available_memory_mb\": 4096"));
        assert!(persisted_json.contains("\"host_used_memory_mb\": 4096"));
        assert!(persisted_json.contains("\"host_cpu_load_1m_x100\": 42"));
        assert!(persisted_json.contains("\"host_cpu_load_5m_x100\": 35"));
        assert!(persisted_json.contains("\"host_cpu_load_15m_x100\": 21"));
        assert!(persisted_json.contains("\"launcher_process_memory_mb\": 128"));
        assert!(persisted_json.contains("\"active_session_count\": 1"));
        assert!(persisted_json.contains("\"estimated_remaining_memory_mb\": 1024"));
        assert!(persisted_json.contains("\"launch_disk_available_mb\": 1536"));
        assert!(persisted_json.contains("\"launch_disk_headroom_mb\": 2048"));
        assert!(persisted_json.contains("\"disk_pressure\": true"));
        assert!(!persisted_json.contains("command"));
        assert!(!persisted_json.contains("-Xmx2048M"));
        assert!(!persisted_json.contains("-Dauth.username"));
        assert!(!persisted_json.contains("Player"));
        assert!(!persisted_json.contains("java_path"));
        assert!(!persisted_json.contains("/usr/bin/java"));
        assert!(!persisted_json.contains("/tmp/natives"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_persists_healing_without_java_path_fragments() {
        let root = test_root("healing-privacy");
        let paths = test_paths(&root);
        let mut record = test_record("healing-privacy");
        let healing = build_healing_summary(HealingSummaryInput {
            auth_mode: "offline",
            requested_java_path: " /home/alice/.sdkman/candidates/java/21/bin/java ",
            requested_preset: "",
            effective_java_path: Some(r"C:\Users\alice\AppData\Local\VendorRuntime\java.exe"),
            effective_preset: None,
            fallback_applied: None,
            retry_count: 0,
            failure_class: None,
        })
        .expect("build healing");
        record.healing = serde_json::to_value(healing).ok();

        let proof = persist_record(&paths, &record, None, "running").expect("persist report");

        assert!(proof.healing.is_some());
        let persisted_json = fs::read_to_string(report_path(&paths, "healing-privacy"))
            .expect("read persisted report");
        let persisted_lower = persisted_json.to_ascii_lowercase();
        for fragment in [
            "/usr",
            "/home",
            "\\",
            "java",
            "alice",
            "sdkman",
            "candidates",
            "bin",
            "users",
            "appdata",
            "vendorruntime",
            "java.exe",
        ] {
            assert!(
                !persisted_lower.contains(fragment),
                "persisted healing leaked fragment {fragment:?}: {persisted_json}"
            );
        }
        assert!(!persisted_json.contains("requested_java_path"));
        assert!(!persisted_json.contains("effective_java_path"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_resource_budget_without_current_pressure_fields_is_invalid() {
        let root = test_root("resource-budget-missing-current-fields");
        let paths = test_paths(&root);
        fs::create_dir_all(report_dir(&paths)).expect("create report dir");
        fs::write(
            report_path(&paths, "resource-budget-missing-current-fields"),
            serde_json::to_string_pretty(&json!({
                "schema": LAUNCH_PROOF_SCHEMA,
                "schema_version": LAUNCH_PROOF_SCHEMA_VERSION,
                "session_id": "resource-budget-missing-current-fields",
                "instance_id": "instance",
                "version_id": "1.21.1",
                "launched_at": "2026-01-01T00:00:00.000Z",
                "recorded_at": "2026-01-01T00:01:00.000Z",
                "outcome": "running",
                "scenario": {
                    "scenario_id": "managed_launch",
                    "performance_mode": "managed",
                    "requested_memory_mb": 4096,
                    "version_id": "1.21.1"
                },
                "device": {
                    "tier": "mid",
                    "total_memory_mb": 16384,
                    "cpu_threads": 6
                },
                "resource_budget": {
                    "host_total_memory_mb": 8192,
                    "host_cpu_threads": 4,
                    "active_session_count": 1,
                    "active_install_count": 0,
                    "active_memory_allocation_mb": 2048,
                    "requested_memory_mb": 4096,
                    "estimated_remaining_memory_mb": 2048,
                    "memory_headroom_mb": 2048,
                    "memory_pressure": false,
                    "cpu_pressure": false,
                    "install_pressure": false
                },
                "stages": []
            }))
            .expect("serialize report"),
        )
        .expect("write report");

        let error = load(&paths, "resource-budget-missing-current-fields")
            .expect_err("missing current resource budget pressure fields should be invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_metadata_accepts_current_modes_and_rejects_aliases() {
        let development = LaunchBenchmarkMetadata::new(None, None, None, Some("development"));
        let qualification = LaunchBenchmarkMetadata::new(None, None, None, Some("qualification"));
        let release = LaunchBenchmarkMetadata::new(None, None, None, Some("release_validation"));
        let alias = LaunchBenchmarkMetadata::new(None, None, None, Some("qual"));
        let different_case = LaunchBenchmarkMetadata::new(None, None, None, Some("QUALIFICATION"));
        let unknown = LaunchBenchmarkMetadata::new(None, None, None, Some("nightly-check"));

        assert_eq!(development.mode.as_deref(), Some("development"));
        assert_eq!(qualification.mode.as_deref(), Some("qualification"));
        assert_eq!(release.mode.as_deref(), Some("release_validation"));
        assert_eq!(alias.mode, None);
        assert_eq!(different_case.mode, None);
        assert_eq!(unknown.mode, None);
    }

    #[test]
    fn normal_launch_proof_context_has_no_benchmark_metadata() {
        let root = test_root("normal-context");
        let paths = test_paths(&root);
        let record = test_record("normal-context");
        let intent = LaunchIntent {
            session_id: "normal-context".to_string(),
            library_dir: root.join("library"),
            instance_id: "instance".to_string(),
            version_id: "1.21.4".to_string(),
            username: "Player".to_string(),
            auth: croopor_launcher::LaunchAuthContext::offline("Player"),
            requested_java: String::new(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 4096,
            min_memory_mb: 1024,
            resolution: None,
            launcher_name: "croopor".to_string(),
            launcher_version: "test".to_string(),
            game_dir: None,
            guardian: croopor_launcher::LaunchGuardianContext::default(),
            performance_mode: "managed".to_string(),
        };
        let context = LaunchProofContext::from_intent(&intent);

        assert_eq!(context.benchmark, None);
        assert_eq!(context.resource_budget, None);

        let proof = persist_record_with_context(&paths, &record, None, "running", Some(&context))
            .expect("persist report");

        assert_eq!(proof.scenario.benchmark_profile, None);
        assert_eq!(proof.scenario.benchmark_run_type, None);
        assert_eq!(proof.scenario.benchmark_mode, None);
        assert_eq!(proof.scenario.benchmark_id, None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_report_maps_scenarios_from_performance_mode() {
        assert_eq!(
            scenario_id_for_performance_mode("managed"),
            "managed_launch"
        );
        assert_eq!(
            scenario_id_for_performance_mode("vanilla"),
            "vanilla_launch"
        );
        assert_eq!(scenario_id_for_performance_mode("custom"), "custom_launch");
        assert_eq!(
            scenario_id_for_performance_mode("unexpected"),
            "unknown_launch"
        );
    }

    #[test]
    fn device_tier_classification_is_host_independent_and_conservative() {
        assert_eq!(classify_device_tier(None, None), "unknown");
        assert_eq!(classify_device_tier(Some(4), Some(32_768)), "low");
        assert_eq!(classify_device_tier(Some(8), Some(8_192)), "low");
        assert_eq!(classify_device_tier(Some(6), Some(16_384)), "mid");
        assert_eq!(classify_device_tier(Some(8), Some(16_384)), "mid");
        assert_eq!(classify_device_tier(Some(12), Some(32_768)), "high");
    }

    fn comparison_report(
        session_id: &str,
        recorded_at: &str,
        completed_stage_duration_ms: u64,
    ) -> LaunchProofRecord {
        LaunchProofRecord {
            schema: LAUNCH_PROOF_SCHEMA.to_string(),
            schema_version: LAUNCH_PROOF_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            instance_id: "instance".to_string(),
            version_id: "1.21.4".to_string(),
            launched_at: recorded_at.to_string(),
            recorded_at: recorded_at.to_string(),
            outcome: "exited".to_string(),
            scenario: LaunchProofScenario {
                scenario_id: "managed_launch".to_string(),
                performance_mode: "managed".to_string(),
                requested_memory_mb: Some(4096),
                version_id: Some("1.21.4".to_string()),
                benchmark_profile: None,
                benchmark_run_type: None,
                benchmark_mode: None,
                benchmark_id: None,
            },
            device: LaunchProofDevice {
                tier: "mid".to_string(),
                total_memory_mb: Some(16_384),
                cpu_threads: Some(6),
            },
            resource_budget: None,
            pid: None,
            exit_code: Some(0),
            boot_duration_ms: None,
            priority: None,
            failure_class: None,
            failure_detail: None,
            guardian: None,
            healing: None,
            stages: vec![LaunchStageRecord {
                stage: "queued".to_string(),
                label: launch_stage_label("queued").to_string(),
                started_at_ms: 1_000,
                ended_at_ms: Some(1_000 + completed_stage_duration_ms),
                duration_ms: Some(completed_stage_duration_ms),
                result: Some("ok".to_string()),
                warnings: Vec::new(),
                fallback_reason: None,
            }],
            comparison: None,
        }
    }

    fn set_benchmark_metadata(
        report: &mut LaunchProofRecord,
        profile: &str,
        run_type: &str,
        mode: &str,
    ) {
        report.scenario.benchmark_profile = Some(profile.to_string());
        report.scenario.benchmark_run_type = Some(run_type.to_string());
        report.scenario.benchmark_mode = Some(mode.to_string());
    }

    fn set_launch_mode(report: &mut LaunchProofRecord, mode: &str) {
        report.scenario.scenario_id = scenario_id_for_performance_mode(mode).to_string();
        report.scenario.performance_mode = mode.to_string();
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
            command: vec!["java".to_string(), "-Xmx2048M".to_string()],
            java_path: Some("/usr/bin/java".to_string()),
            natives_dir: Some("/tmp/natives".to_string()),
            failure: None,
            healing: Some(json!({ "fallback_applied": "test fallback" })),
            guardian: Some(json!({ "mode": "managed" })),
            stages: vec![LaunchStageRecord {
                stage: "queued".to_string(),
                label: launch_stage_label("queued").to_string(),
                started_at_ms: 1,
                ended_at_ms: Some(2),
                duration_ms: Some(1),
                result: Some("ok".to_string()),
                warnings: Vec::new(),
                fallback_reason: None,
            }],
        }
    }

    fn test_record_with_stage_duration(session_id: &str, duration_ms: u64) -> LaunchSessionRecord {
        let mut record = test_record(session_id);
        record.stages[0].started_at_ms = 1_000;
        record.stages[0].ended_at_ms = Some(1_000 + duration_ms);
        record.stages[0].duration_ms = Some(duration_ms);
        record
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "croopor-launch-reports-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
