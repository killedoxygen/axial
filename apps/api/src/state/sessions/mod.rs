mod classify;
mod priority;
mod supervisor;

use croopor_launcher::{
    LaunchEvent, LaunchFailure, LaunchFailureClass, LaunchLogEvent, LaunchNotice,
    LaunchPriorityEvidence, LaunchSessionOutcomeKind, LaunchSessionRecord, LaunchStageEvidence,
    LaunchStageRecord, LaunchState, LaunchStatusEvent, classify_startup_failure_text,
    launch_notice_from_values, launch_stage_label, launch_state_name,
};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, broadcast};

const MAX_GUARDIAN_STAGE_DETAILS: usize = 8;
const MAX_STAGE_EVIDENCE: usize = 16;
const MAX_STAGE_EVIDENCE_DETAILS: usize = 8;
const MAX_STAGE_NOTE_CHARS: usize = 160;
const MAX_NOTICE_MESSAGE_CHARS: usize = 180;
const MAX_NOTICE_DETAIL_CHARS: usize = 240;
const MAX_NOTICE_DETAILS: usize = 8;
const PRIVATE_NOTICE_FALLBACK: &str = "Launch status details were hidden for privacy.";

struct SessionEntry {
    record: LaunchSessionRecord,
    events: broadcast::Sender<LaunchEvent>,
    child: Option<Arc<Mutex<Child>>>,
    startup_observed: Arc<AtomicBool>,
    boot_completed: Arc<AtomicBool>,
    log_count: Arc<AtomicUsize>,
    observed_failure: Option<LaunchFailureClass>,
    stop_requested: bool,
}

pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionEntry>>,
    changes: broadcast::Sender<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupOutcome {
    Stable,
    Exited,
    TimedOut,
    Stalled,
}

impl SessionStore {
    pub fn new() -> Self {
        let (changes, _) = broadcast::channel(64);
        Self {
            sessions: RwLock::new(HashMap::new()),
            changes,
        }
    }

    fn notify_changed(&self) {
        let _ = self.changes.send(());
    }

    pub async fn insert(&self, mut record: LaunchSessionRecord) {
        let (events, _) = broadcast::channel(256);
        ensure_stage_started(&mut record, now_ms());
        let mut sessions = self.sessions.write().await;
        sessions.insert(
            record.session_id.0.clone(),
            SessionEntry {
                record,
                events,
                child: None,
                startup_observed: Arc::new(AtomicBool::new(false)),
                boot_completed: Arc::new(AtomicBool::new(false)),
                log_count: Arc::new(AtomicUsize::new(0)),
                observed_failure: None,
                stop_requested: false,
            },
        );
        drop(sessions);
        self.notify_changed();
    }

    pub async fn get(&self, session_id: &str) -> Option<LaunchSessionRecord> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.record.clone())
    }

    pub async fn attach_benchmark(
        &self,
        session_id: &str,
        benchmark: serde_json::Value,
    ) -> Option<LaunchSessionRecord> {
        let mut sessions = self.sessions.write().await;
        let entry = sessions.get_mut(session_id)?;
        entry.record.benchmark = Some(benchmark);
        let record = entry.record.clone();
        drop(sessions);
        self.notify_changed();
        Some(record)
    }

    pub async fn subscribe(&self, session_id: &str) -> Option<broadcast::Receiver<LaunchEvent>> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.events.subscribe())
    }

    pub fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    pub async fn emit_log(
        &self,
        session_id: &str,
        source: impl Into<String>,
        text: impl Into<String>,
    ) {
        let source = source.into();
        let text = text.into();
        let mut sessions = self.sessions.write().await;
        let mut changed = false;
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.startup_observed.store(true, Ordering::Relaxed);
            entry.log_count.fetch_add(1, Ordering::Relaxed);
            if classify::boot_marker_detected(&text) && record_boot_completion(entry, now_ms()) {
                entry.observed_failure = None;
                let pid = entry.record.pid;
                match priority::promote_after_boot(pid) {
                    Ok(promotion) => {
                        record_priority_promotion(entry, promotion.proof_value(), None);
                    }
                    Err(error) => {
                        let promotion_error = priority::sanitize_priority_error(&error);
                        record_priority_promotion(entry, "failed", promotion_error);
                        tracing::warn!(
                            session_id,
                            pid,
                            error = %error,
                            "failed to promote launched game process after boot marker"
                        );
                    }
                }
            }
            if entry.boot_completed.load(Ordering::Relaxed)
                && matches!(
                    entry.record.state,
                    LaunchState::Starting | LaunchState::Monitoring
                )
            {
                let mut status = LaunchStatusEvent {
                    state: "running".to_string(),
                    benchmark: entry.record.benchmark.clone(),
                    pid: entry.record.pid,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: entry.record.healing.clone(),
                    guardian: entry.record.guardian.clone(),
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                };
                apply_status_update(entry, &mut status);
                let _ = entry.events.send(LaunchEvent::Status(status));
                changed = true;
            }
            let failure_class = classify_startup_failure_text(&text);
            if failure_class != LaunchFailureClass::Unknown {
                entry.observed_failure = Some(failure_class);
            }
            let _ = entry
                .events
                .send(LaunchEvent::Log(LaunchLogEvent { source, text }));
        }
        drop(sessions);
        if changed {
            self.notify_changed();
        }
    }

    pub async fn emit_status(&self, session_id: &str, mut event: LaunchStatusEvent) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            apply_status_update(entry, &mut event);
            let _ = entry.events.send(LaunchEvent::Status(event));
            drop(sessions);
            self.notify_changed();
        }
    }

    pub async fn record_stage_evidence(
        &self,
        session_id: &str,
        evidence: Vec<LaunchStageEvidence>,
    ) -> Option<LaunchSessionRecord> {
        let mut sessions = self.sessions.write().await;
        let entry = sessions.get_mut(session_id)?;
        ensure_stage_started(&mut entry.record, now_ms());
        let evidence = sanitize_stage_evidence(evidence);
        apply_stage_evidence(entry.record.stages.last_mut(), &evidence);
        let record = entry.record.clone();
        drop(sessions);
        self.notify_changed();
        Some(record)
    }

    pub async fn start_process(
        self: &Arc<Self>,
        mut record: LaunchSessionRecord,
        mut command: Command,
    ) -> std::io::Result<LaunchSessionRecord> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let priority = match priority::configure_start_priority(&mut command) {
            Ok(mode) => LaunchPriorityEvidence {
                start_mode: mode.proof_value().to_string(),
                start_error: None,
                promotion: None,
                promotion_error: None,
            },
            Err(error) => {
                let start_error = priority::sanitize_priority_error(&error);
                tracing::warn!(
                    session_id = %record.session_id.0,
                    error = %error,
                    "failed to configure launch process priority; continuing with default priority"
                );
                LaunchPriorityEvidence {
                    start_mode: "default_after_setup_error".to_string(),
                    start_error,
                    promotion: None,
                    promotion_error: None,
                }
            }
        };
        record.priority = Some(priority.clone());
        self.record_priority_start(&record.session_id.0, priority)
            .await;
        let child = command.spawn()?;
        let process_started_at_ms = now_ms();
        record.pid = child.id();
        record.process_started_at_ms = Some(process_started_at_ms);
        record.boot_completed_at_ms = None;
        record.boot_duration_ms = None;
        record.state = LaunchState::Starting;

        let session_id = record.session_id.0.clone();
        let child_handle = Arc::new(Mutex::new(child));
        let mut startup_observed = Arc::new(AtomicBool::new(false));
        {
            let mut sessions = self.sessions.write().await;
            if let Some(entry) = sessions.get_mut(&session_id) {
                let previous_state = entry.record.state;
                let previous_stages = entry.record.stages.clone();
                let previous_benchmark = entry.record.benchmark.clone();
                record.failure = None;
                record.outcome = None;
                let mut stored_record = record.clone();
                stored_record.state = previous_state;
                stored_record.stages = previous_stages;
                if stored_record.benchmark.is_none() {
                    stored_record.benchmark = previous_benchmark;
                }
                ensure_stage_started(&mut stored_record, now_ms());
                entry.record = stored_record;
                entry.child = Some(child_handle.clone());
                startup_observed = entry.startup_observed.clone();
                entry.startup_observed.store(false, Ordering::Relaxed);
                entry.boot_completed.store(false, Ordering::Relaxed);
                entry.log_count.store(0, Ordering::Relaxed);
                entry.observed_failure = None;
                entry.stop_requested = false;
            } else {
                let (events, _) = broadcast::channel(256);
                sessions.insert(
                    session_id.clone(),
                    SessionEntry {
                        record: {
                            let mut record = record.clone();
                            ensure_stage_started(&mut record, now_ms());
                            record
                        },
                        events,
                        child: Some(child_handle.clone()),
                        startup_observed: startup_observed.clone(),
                        boot_completed: Arc::new(AtomicBool::new(false)),
                        log_count: Arc::new(AtomicUsize::new(0)),
                        observed_failure: None,
                        stop_requested: false,
                    },
                );
            }
        }
        self.notify_changed();

        self.emit_status(
            &session_id,
            LaunchStatusEvent {
                state: "starting".to_string(),
                benchmark: record.benchmark.clone(),
                pid: record.pid,
                exit_code: None,
                failure_class: None,
                failure_detail: None,
                healing: record.healing.clone(),
                guardian: record.guardian.clone(),
                outcome: None,
                notice: None,
                evidence: Vec::new(),
                stages: Vec::new(),
            },
        )
        .await;

        supervisor::spawn_output_tasks(self.clone(), session_id.clone(), child_handle.clone())
            .await;
        supervisor::spawn_startup_watchdog(
            self.clone(),
            session_id.clone(),
            child_handle.clone(),
            startup_observed,
        );
        supervisor::spawn_wait_task(self.clone(), session_id, child_handle);

        Ok(record)
    }

    async fn record_priority_start(&self, session_id: &str, priority: LaunchPriorityEvidence) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.record.priority = Some(priority);
        }
    }

    pub async fn kill(&self, session_id: &str) -> std::io::Result<()> {
        let child = {
            let mut sessions = self.sessions.write().await;
            let Some(entry) = sessions.get_mut(session_id) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "session not found",
                ));
            };
            entry.stop_requested = true;
            entry.child.clone()
        };
        if let Some(child) = child {
            child.lock().await.kill().await
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not found",
            ))
        }
    }

    pub async fn remove(&self, session_id: &str) {
        self.sessions.write().await.remove(session_id);
        self.notify_changed();
    }

    pub async fn observed_failure(&self, session_id: &str) -> Option<LaunchFailureClass> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.observed_failure)
    }

    pub async fn terminate_all(&self) {
        let children = self
            .sessions
            .read()
            .await
            .values()
            .filter_map(|entry| entry.child.clone())
            .collect::<Vec<_>>();

        for child in children {
            let _ = child.lock().await.kill().await;
        }

        self.sessions.write().await.clear();
        self.notify_changed();
    }

    pub async fn active_records(&self) -> Vec<LaunchSessionRecord> {
        self.sessions
            .read()
            .await
            .values()
            .filter(|entry| !classify::is_terminal_state(entry.record.state))
            .map(|entry| entry.record.clone())
            .collect()
    }

    pub async fn has_active_instance(&self, instance_id: &str) -> bool {
        self.sessions.read().await.values().any(|entry| {
            entry.record.instance_id == instance_id
                && !classify::is_terminal_state(entry.record.state)
        })
    }

    pub async fn has_any_active_session_id<'a>(
        &self,
        session_ids: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let session_ids = session_ids.into_iter().collect::<HashSet<_>>();
        if session_ids.is_empty() {
            return false;
        }

        let sessions = self.sessions.read().await;
        session_ids.into_iter().any(|session_id| {
            sessions
                .get(session_id)
                .is_some_and(|entry| !classify::is_terminal_state(entry.record.state))
        })
    }

    pub async fn active_session_count(&self) -> usize {
        self.sessions
            .read()
            .await
            .values()
            .filter(|entry| !classify::is_terminal_state(entry.record.state))
            .count()
    }

    pub async fn active_memory_allocation_mb(&self) -> u64 {
        self.sessions
            .read()
            .await
            .values()
            .filter(|entry| !classify::is_terminal_state(entry.record.state))
            .filter_map(|entry| command_xmx_mb(&entry.record.command))
            .sum()
    }

    pub async fn first_active_version<'a>(
        &self,
        version_ids: impl IntoIterator<Item = &'a str>,
    ) -> Option<String> {
        let targets: std::collections::HashSet<&str> = version_ids.into_iter().collect();
        self.sessions
            .read()
            .await
            .values()
            .find(|entry| {
                targets.contains(entry.record.version_id.as_str())
                    && !classify::is_terminal_state(entry.record.state)
            })
            .map(|entry| entry.record.version_id.clone())
    }

    pub async fn wait_for_startup(&self, session_id: &str, timeout: Duration) -> StartupOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = self.sessions.read().await.get(session_id).map(|entry| {
                (
                    entry.record.state,
                    entry.boot_completed.load(Ordering::Relaxed),
                    entry.log_count.load(Ordering::Relaxed),
                )
            });

            let Some((state, boot_completed, log_count)) = snapshot else {
                return StartupOutcome::Exited;
            };
            if boot_completed {
                return StartupOutcome::Stable;
            }
            if classify::is_terminal_state(state) {
                return StartupOutcome::Exited;
            }
            if tokio::time::Instant::now() >= deadline {
                return if log_count == 0 {
                    StartupOutcome::Stalled
                } else {
                    StartupOutcome::TimedOut
                };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn apply_status_update(entry: &mut SessionEntry, event: &mut LaunchStatusEvent) {
    let now = now_ms();
    let previous_state = entry.record.state;
    let next_state = classify::parse_launch_state(&event.state);
    let parsed_failure_class = event
        .failure_class
        .as_deref()
        .map(classify::parse_failure_class);
    event.failure_detail = event
        .failure_detail
        .take()
        .and_then(|detail| sanitize_public_notice_text(&detail, MAX_NOTICE_DETAIL_CHARS));
    event.guardian = event.guardian.take().and_then(sanitize_public_json_value);
    event.healing = event.healing.take().and_then(sanitize_public_json_value);
    if event.outcome.is_none() {
        event.outcome = entry.record.outcome.clone().or_else(|| {
            classify::classify_session_outcome(classify::SessionOutcomeInput {
                previous_state,
                next_state,
                boot_completed: entry.boot_completed.load(Ordering::Relaxed)
                    || entry.record.boot_completed_at_ms.is_some(),
                stop_requested: entry.stop_requested,
                exit_code: event.exit_code,
                failure_class: parsed_failure_class,
            })
        });
    }

    update_stage_history(&mut entry.record, event, now);

    entry.record.state = next_state;
    if event.pid.is_some() {
        entry.record.pid = event.pid;
    }
    if event.exit_code.is_some() {
        entry.record.exit_code = event.exit_code;
    }
    if let Some(failure_class) = parsed_failure_class {
        entry.record.failure = Some(LaunchFailure {
            class: failure_class,
            detail: event.failure_detail.clone(),
        });
    }
    if event.healing.is_some() {
        entry.record.healing = event.healing.clone();
    }
    if event.guardian.is_some() {
        entry.record.guardian = event.guardian.clone();
    }
    if event.benchmark.is_some() {
        entry.record.benchmark = event.benchmark.clone();
    } else {
        event.benchmark = entry.record.benchmark.clone();
    }
    if let Some(outcome) = &event.outcome {
        entry.record.outcome = Some(outcome.clone());
    }
    if event.notice.is_none() {
        event.notice = launch_notice_from_values(
            event.guardian.as_ref(),
            event.healing.as_ref(),
            event.outcome.as_ref(),
            event.failure_detail.as_deref(),
            None,
        );
    }
    if let Some(notice) = event.notice.take() {
        event.notice = Some(sanitize_launch_notice(notice));
    }
    event.stages = entry.record.stages.clone();
}

fn record_boot_completion(entry: &mut SessionEntry, now: u64) -> bool {
    if entry.boot_completed.swap(true, Ordering::Relaxed) {
        return false;
    }
    entry.record.boot_completed_at_ms = Some(now);
    entry.record.boot_duration_ms = entry
        .record
        .process_started_at_ms
        .map(|started_at| now.saturating_sub(started_at));
    true
}

fn record_priority_promotion(
    entry: &mut SessionEntry,
    promotion: &str,
    promotion_error: Option<String>,
) {
    let priority = entry
        .record
        .priority
        .get_or_insert_with(default_priority_evidence);
    priority.promotion = Some(promotion.to_string());
    priority.promotion_error = promotion_error;
}

fn default_priority_evidence() -> LaunchPriorityEvidence {
    LaunchPriorityEvidence {
        start_mode: platform_default_start_mode().to_string(),
        start_error: None,
        promotion: None,
        promotion_error: None,
    }
}

fn platform_default_start_mode() -> &'static str {
    #[cfg(windows)]
    {
        "below_normal_until_boot"
    }
    #[cfg(not(windows))]
    {
        "noop"
    }
}

fn ensure_stage_started(record: &mut LaunchSessionRecord, now: u64) {
    if record.stages.is_empty() {
        let stage = launch_state_name(record.state).to_string();
        record.stages.push(start_stage(&stage, now, None, None));
    }
}

fn update_stage_history(record: &mut LaunchSessionRecord, event: &mut LaunchStatusEvent, now: u64) {
    ensure_stage_started(record, now);

    let next_stage = event.state.as_str();
    let terminal_result = terminal_stage_result(event);
    let (warnings, fallback_reason) = stage_notes(event);
    let evidence = sanitize_stage_evidence(std::mem::take(&mut event.evidence));
    let current_stage = record
        .stages
        .last()
        .map(|stage| stage.stage.as_str())
        .unwrap_or_default();
    if current_stage == next_stage {
        apply_stage_notes(
            record.stages.last_mut(),
            &warnings,
            fallback_reason.as_deref(),
        );
        apply_stage_evidence(record.stages.last_mut(), &evidence);
        if let Some(result) = terminal_result {
            close_open_stage(record.stages.last_mut(), now, result);
        }
        return;
    }

    let previous_result = terminal_result.unwrap_or("ok");
    close_open_stage(record.stages.last_mut(), now, previous_result);
    let mut next = start_stage(next_stage, now, Some(warnings), fallback_reason);
    apply_stage_evidence(Some(&mut next), &evidence);
    if let Some(result) = terminal_result {
        close_open_stage(Some(&mut next), now, result);
    }
    record.stages.push(next);
}

fn start_stage(
    stage: &str,
    now: u64,
    warnings: Option<Vec<String>>,
    fallback_reason: Option<String>,
) -> LaunchStageRecord {
    LaunchStageRecord {
        stage: stage.to_string(),
        label: launch_stage_label(stage).to_string(),
        started_at_ms: now,
        ended_at_ms: None,
        duration_ms: None,
        result: None,
        warnings: warnings.unwrap_or_default(),
        fallback_reason,
        evidence: Vec::new(),
    }
}

fn close_open_stage(stage: Option<&mut LaunchStageRecord>, now: u64, result: &str) {
    let Some(stage) = stage else {
        return;
    };
    if stage.ended_at_ms.is_some() {
        return;
    }
    stage.ended_at_ms = Some(now);
    stage.duration_ms = Some(now.saturating_sub(stage.started_at_ms));
    stage.result = Some(result.to_string());
}

fn terminal_stage_result(event: &LaunchStatusEvent) -> Option<&'static str> {
    match event.state.as_str() {
        "failed" => Some("failed"),
        "exited"
            if event.failure_class.is_some()
                || event.outcome.as_ref().is_some_and(|outcome| {
                    matches!(outcome.kind, LaunchSessionOutcomeKind::Failed)
                }) =>
        {
            Some("failed")
        }
        "exited" => Some("exited"),
        _ => None,
    }
}

fn stage_notes(event: &LaunchStatusEvent) -> (Vec<String>, Option<String>) {
    let mut warnings = Vec::new();

    if let Some(guardian) = event.guardian.as_ref().and_then(|value| value.as_object())
        && guardian
            .get("decision")
            .and_then(|value| value.as_str())
            .is_some_and(|decision| matches!(decision, "warned" | "intervened" | "blocked"))
    {
        let mut added_guardian_details = 0;
        for detail in guardian
            .get("details")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
        {
            if push_unique_warning(&mut warnings, detail) {
                added_guardian_details += 1;
            }
            if added_guardian_details >= MAX_GUARDIAN_STAGE_DETAILS {
                break;
            }
        }
    }

    let fallback_reason = event
        .healing
        .as_ref()
        .and_then(|value| value.as_object())
        .and_then(|healing| {
            for warning in healing
                .get("warnings")
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str())
            {
                push_unique_warning(&mut warnings, warning);
            }
            healing
                .get("fallback_applied")
                .and_then(|value| value.as_str())
                .and_then(|value| sanitize_public_stage_text(value, MAX_STAGE_NOTE_CHARS))
        });
    (warnings, fallback_reason)
}

fn push_unique_warning(warnings: &mut Vec<String>, warning: &str) -> bool {
    let Some(warning) = sanitize_public_stage_text(warning, MAX_STAGE_NOTE_CHARS) else {
        return false;
    };
    if warnings.iter().any(|existing| existing == &warning) {
        return false;
    }
    warnings.push(warning);
    true
}

fn apply_stage_notes(
    stage: Option<&mut LaunchStageRecord>,
    warnings: &[String],
    fallback_reason: Option<&str>,
) {
    let Some(stage) = stage else {
        return;
    };
    for warning in warnings {
        push_unique_warning(&mut stage.warnings, warning);
    }
    if stage.fallback_reason.is_none()
        && let Some(fallback_reason) = fallback_reason
            .and_then(|value| sanitize_public_stage_text(value, MAX_STAGE_NOTE_CHARS))
    {
        stage.fallback_reason = Some(fallback_reason);
    }
}

fn apply_stage_evidence(stage: Option<&mut LaunchStageRecord>, evidence: &[LaunchStageEvidence]) {
    let Some(stage) = stage else {
        return;
    };
    for evidence in evidence {
        if stage
            .evidence
            .iter()
            .any(|existing| existing.id == evidence.id && existing.system == evidence.system)
        {
            continue;
        }
        if stage.evidence.len() >= MAX_STAGE_EVIDENCE {
            break;
        }
        stage.evidence.push(evidence.clone());
    }
}

fn sanitize_stage_evidence(evidence: Vec<LaunchStageEvidence>) -> Vec<LaunchStageEvidence> {
    evidence
        .into_iter()
        .filter_map(|evidence| {
            let id = crate::observability::sanitize_evidence_token(
                &evidence.id,
                crate::observability::RedactionAudience::UserVisible,
                64,
            )?;
            let system = crate::observability::sanitize_evidence_token(
                &evidence.system,
                crate::observability::RedactionAudience::UserVisible,
                32,
            )?;
            let summary = crate::observability::sanitize_evidence_text(
                &evidence.summary,
                crate::observability::RedactionAudience::UserVisible,
                160,
            )?;
            let details = evidence
                .details
                .into_iter()
                .filter_map(|detail| {
                    crate::observability::sanitize_evidence_text(
                        &detail,
                        crate::observability::RedactionAudience::UserVisible,
                        120,
                    )
                })
                .take(MAX_STAGE_EVIDENCE_DETAILS)
                .collect();
            Some(LaunchStageEvidence {
                id,
                system,
                summary,
                details,
            })
        })
        .take(MAX_STAGE_EVIDENCE)
        .collect()
}

fn sanitize_launch_notice(notice: LaunchNotice) -> LaunchNotice {
    let message = sanitize_public_notice_text(&notice.message, MAX_NOTICE_MESSAGE_CHARS)
        .unwrap_or_else(|| PRIVATE_NOTICE_FALLBACK.to_string());
    let details = notice
        .details
        .into_iter()
        .filter_map(|detail| sanitize_public_notice_text(&detail, MAX_NOTICE_DETAIL_CHARS))
        .take(MAX_NOTICE_DETAILS)
        .collect::<Vec<_>>();
    let detail = details.first().cloned();
    LaunchNotice {
        message,
        detail,
        details,
        tone: notice.tone,
    }
}

fn sanitize_public_notice_text(value: &str, max_chars: usize) -> Option<String> {
    crate::observability::sanitize_evidence_text(
        value,
        crate::observability::RedactionAudience::UserVisible,
        max_chars,
    )
}

fn sanitize_public_stage_text(value: &str, max_chars: usize) -> Option<String> {
    crate::observability::sanitize_evidence_text(
        value,
        crate::observability::RedactionAudience::UserVisible,
        max_chars,
    )
}

fn sanitize_public_json_value(value: serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            Some(value)
        }
        serde_json::Value::String(value) => {
            sanitize_public_notice_text(&value, MAX_NOTICE_DETAIL_CHARS)
                .map(serde_json::Value::String)
        }
        serde_json::Value::Array(values) => {
            let values = values
                .into_iter()
                .filter_map(sanitize_public_json_value)
                .collect::<Vec<_>>();
            Some(serde_json::Value::Array(values))
        }
        serde_json::Value::Object(values) => {
            let mut sanitized = serde_json::Map::new();
            for (key, value) in values {
                let Some(key) = crate::observability::sanitize_evidence_token(
                    &key,
                    crate::observability::RedactionAudience::UserVisible,
                    64,
                ) else {
                    continue;
                };
                if let Some(value) = sanitize_public_json_value(value) {
                    sanitized.insert(key, value);
                }
            }
            Some(serde_json::Value::Object(sanitized))
        }
    }
}

fn command_xmx_mb(command: &[String]) -> Option<u64> {
    command.iter().rev().find_map(|arg| {
        let value = arg.strip_prefix("-Xmx")?;
        let value = value
            .strip_suffix('M')
            .or_else(|| value.strip_suffix('m'))?;
        value.parse::<u64>().ok()
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or_default()
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use croopor_launcher::{LaunchSessionExitReason, LaunchStageEvidence, SessionId};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::process::Command;

    #[tokio::test]
    async fn launch_stage_history_tracks_transitions_results_and_healing_notes() {
        let store = SessionStore::new();
        store.insert(test_record("stage-history")).await;

        let initial = store.get("stage-history").await.expect("initial record");
        assert_eq!(initial.stages.len(), 1);
        assert_eq!(initial.stages[0].stage, "queued");
        assert_eq!(initial.stages[0].result, None);

        let mut receiver = store.subscribe("stage-history").await.expect("subscribe");
        store
            .emit_status(
                "stage-history",
                LaunchStatusEvent {
                    state: "validating".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: Some(json!({
                        "warnings": ["Requested Java override was bypassed"],
                        "fallback_applied": "Guardian switched to managed Java before launch"
                    })),
                    guardian: None,
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let emitted = receiver.recv().await.expect("status event");
        let LaunchEvent::Status(status) = emitted else {
            panic!("expected status event");
        };
        assert_eq!(status.stages.len(), 2);
        assert_eq!(status.stages[0].stage, "queued");
        assert_eq!(status.stages[0].result.as_deref(), Some("ok"));
        assert_eq!(status.stages[1].stage, "validating");
        assert_eq!(
            status.stages[1].fallback_reason.as_deref(),
            Some("Guardian switched to managed Java before launch")
        );
        assert_eq!(
            status.stages[1].warnings,
            vec!["Requested Java override was bypassed"]
        );

        store
            .emit_status(
                "stage-history",
                LaunchStatusEvent {
                    state: "exited".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: Some(-1),
                    failure_class: Some("startup_stalled".to_string()),
                    failure_detail: Some("no startup activity observed".to_string()),
                    healing: None,
                    guardian: None,
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let terminal = store.get("stage-history").await.expect("terminal record");
        assert_eq!(terminal.stages.len(), 3);
        assert_eq!(terminal.stages[1].stage, "validating");
        assert_eq!(terminal.stages[1].result.as_deref(), Some("failed"));
        assert!(terminal.stages[1].ended_at_ms.is_some());
        assert!(terminal.stages[1].duration_ms.is_some());
        assert_eq!(terminal.stages[2].stage, "exited");
        assert_eq!(terminal.stages[2].result.as_deref(), Some("failed"));
        assert!(terminal.stages[2].ended_at_ms.is_some());
    }

    #[tokio::test]
    async fn launch_stage_history_captures_guardian_details_before_healing_warnings() {
        let store = SessionStore::new();
        store.insert(test_record("guardian-stage-notes")).await;

        let mut receiver = store
            .subscribe("guardian-stage-notes")
            .await
            .expect("subscribe");
        store
            .emit_status(
                "guardian-stage-notes",
                LaunchStatusEvent {
                    state: "validating".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: Some(json!({
                        "warnings": [
                            "Java override was bypassed",
                            "Healing added fallback context"
                        ],
                        "fallback_applied": "Guardian switched to managed Java before launch"
                    })),
                    guardian: Some(json!({
                        "mode": "managed",
                        "decision": "warned",
                        "message": "Guardian flagged launch settings for review.",
                        "details": [
                            "Launch memory budget is tight",
                            "Java override was bypassed",
                            "Launch memory budget is tight",
                            "",
                            42
                        ]
                    })),
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let emitted = receiver.recv().await.expect("status event");
        let LaunchEvent::Status(status) = emitted else {
            panic!("expected status event");
        };
        assert_eq!(
            status.stages[1].warnings,
            vec![
                "Launch memory budget is tight",
                "Java override was bypassed",
                "Healing added fallback context",
            ]
        );
        assert_eq!(
            status.stages[1].fallback_reason.as_deref(),
            Some("Guardian switched to managed Java before launch")
        );

        let stored = store
            .get("guardian-stage-notes")
            .await
            .expect("stored record");
        assert_eq!(stored.stages[1].warnings, status.stages[1].warnings);
        assert_eq!(
            stored.stages[1].fallback_reason.as_deref(),
            Some("Guardian switched to managed Java before launch")
        );
    }

    #[tokio::test]
    async fn launch_status_public_notice_and_stage_notes_redact_sensitive_details() {
        let store = SessionStore::new();
        store.insert(test_record("guardian-stage-redaction")).await;

        let mut receiver = store
            .subscribe("guardian-stage-redaction")
            .await
            .expect("subscribe");
        store
            .emit_status(
                "guardian-stage-redaction",
                LaunchStatusEvent {
                    state: "validating".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: Some(json!({
                        "warnings": [
                            "Healing added safe fallback context",
                            r#"Fallback used C:\Users\Alice\.jdks\java.exe --accessToken raw-secret-token -Xmx8192M"#
                        ],
                        "fallback_applied": "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer"
                    })),
                    guardian: Some(json!({
                        "mode": "managed",
                        "decision": "blocked",
                        "message": "/home/alice/.minecraft/java.exe --accessToken raw-secret-token",
                        "details": [
                            "Review the latest game log before retrying",
                            r#"Java failed at C:\Users\Alice\AppData\java.exe -Xmx8192M -Dtoken=raw"#,
                            "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer"
                        ]
                    })),
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let emitted = receiver.recv().await.expect("status event");
        let LaunchEvent::Status(status) = emitted else {
            panic!("expected status event");
        };
        let notice = status.notice.as_ref().expect("public notice");
        assert_eq!(notice.message, "Guardian blocked an unsafe launch setup.");
        assert_eq!(
            notice.details,
            vec!["Review the latest game log before retrying."]
        );
        assert_eq!(
            status.stages[1].warnings,
            vec![
                "Review the latest game log before retrying",
                "Healing added safe fallback context",
            ]
        );
        assert_eq!(status.stages[1].fallback_reason, None);

        let stored = store
            .get("guardian-stage-redaction")
            .await
            .expect("stored record");
        assert_eq!(stored.stages[1].warnings, status.stages[1].warnings);
        assert_eq!(stored.stages[1].fallback_reason, None);
        assert_public_session_payload_excludes_sensitive_content(
            &serde_json::to_string(&status).expect("status json"),
        );
        assert_public_session_payload_excludes_sensitive_content(
            &serde_json::to_string(&stored.stages).expect("stage json"),
        );
    }

    #[tokio::test]
    async fn launch_stage_history_merges_sanitized_stage_evidence() {
        let store = SessionStore::new();
        store.insert(test_record("stage-evidence")).await;

        let mut receiver = store.subscribe("stage-evidence").await.expect("subscribe");
        store
            .emit_status(
                "stage-evidence",
                LaunchStatusEvent {
                    state: "preparing".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: None,
                    guardian: None,
                    outcome: None,
                    notice: None,
                    evidence: vec![
                        LaunchStageEvidence {
                            id: "execution_launch_command_prepared".to_string(),
                            system: "execution".to_string(),
                            summary: "Execution prepared a runnable launch command.".to_string(),
                            details: vec![
                                "arg_count:3".to_string(),
                                r"program:C:\Users\Alice\.jdks\java.exe".to_string(),
                                "-Xmx8192M".to_string(),
                            ],
                        },
                        LaunchStageEvidence {
                            id: r"bad\path".to_string(),
                            system: "execution".to_string(),
                            summary: "/home/alice/.minecraft leaked".to_string(),
                            details: vec!["token=secret".to_string()],
                        },
                    ],
                    stages: Vec::new(),
                },
            )
            .await;

        let emitted = receiver.recv().await.expect("status event");
        let LaunchEvent::Status(status) = emitted else {
            panic!("expected status event");
        };
        assert_eq!(status.stages[1].evidence.len(), 1);
        assert_eq!(
            status.stages[1].evidence[0].id,
            "execution_launch_command_prepared"
        );
        assert_eq!(status.stages[1].evidence[0].details, vec!["arg_count:3"]);

        let stored = store.get("stage-evidence").await.expect("stored record");
        let encoded = serde_json::to_string(&stored.stages).expect("stage json");
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("-Xmx"));
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("/home/"));
    }

    #[tokio::test]
    async fn launch_stage_history_ignores_allowed_empty_and_malformed_guardian_notes() {
        let store = SessionStore::new();

        store.insert(test_record("guardian-allowed")).await;
        store
            .emit_status(
                "guardian-allowed",
                LaunchStatusEvent {
                    state: "validating".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: None,
                    guardian: Some(json!({
                        "mode": "managed",
                        "decision": "allowed"
                    })),
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let allowed = store.get("guardian-allowed").await.expect("stored record");
        assert!(allowed.stages[1].warnings.is_empty());
        assert_eq!(allowed.stages[1].fallback_reason, None);

        store.insert(test_record("guardian-empty")).await;
        store
            .emit_status(
                "guardian-empty",
                LaunchStatusEvent {
                    state: "validating".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: Some(json!("not an object")),
                    guardian: Some(json!({
                        "mode": "managed",
                        "decision": "blocked",
                        "details": []
                    })),
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let empty = store.get("guardian-empty").await.expect("stored record");
        assert!(empty.stages[1].warnings.is_empty());
        assert_eq!(empty.stages[1].fallback_reason, None);

        store.insert(test_record("guardian-malformed")).await;
        store
            .emit_status(
                "guardian-malformed",
                LaunchStatusEvent {
                    state: "validating".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: None,
                    guardian: Some(json!("not an object")),
                    outcome: None,
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        let malformed = store
            .get("guardian-malformed")
            .await
            .expect("stored record");
        assert!(malformed.stages[1].warnings.is_empty());
        assert_eq!(malformed.stages[1].fallback_reason, None);
    }

    #[test]
    fn launch_stage_notes_bounds_unique_guardian_details() {
        let details = (0..MAX_GUARDIAN_STAGE_DETAILS + 3)
            .map(|index| format!("Guardian detail {index}"))
            .collect::<Vec<_>>();
        let event = LaunchStatusEvent {
            state: "validating".to_string(),
            benchmark: None,
            pid: None,
            exit_code: None,
            failure_class: None,
            failure_detail: None,
            healing: None,
            guardian: Some(json!({
                "mode": "managed",
                "decision": "intervened",
                "details": details
            })),
            outcome: None,
            notice: None,
            evidence: Vec::new(),
            stages: Vec::new(),
        };

        let (warnings, fallback_reason) = stage_notes(&event);
        assert_eq!(warnings.len(), MAX_GUARDIAN_STAGE_DETAILS);
        assert_eq!(warnings[0], "Guardian detail 0");
        assert_eq!(
            warnings[MAX_GUARDIAN_STAGE_DETAILS - 1],
            format!("Guardian detail {}", MAX_GUARDIAN_STAGE_DETAILS - 1)
        );
        assert_eq!(fallback_reason, None);
    }

    #[tokio::test]
    async fn launch_status_events_preserve_attached_benchmark_metadata() {
        let store = SessionStore::new();
        store.insert(test_record("benchmark-status")).await;
        let benchmark = json!({
            "id": "benchmark-status",
            "profile": "dev-default",
            "run_type": "repeat",
            "mode": "release_validation"
        });
        store
            .attach_benchmark("benchmark-status", benchmark.clone())
            .await
            .expect("attached benchmark");

        let mut receiver = store
            .subscribe("benchmark-status")
            .await
            .expect("subscribe");
        store
            .emit_status(
                "benchmark-status",
                LaunchStatusEvent {
                    state: "validating".to_string(),
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
                },
            )
            .await;

        let emitted = receiver.recv().await.expect("status event");
        let LaunchEvent::Status(status) = emitted else {
            panic!("expected status event");
        };
        assert_eq!(status.benchmark, Some(benchmark.clone()));
        let record = store.get("benchmark-status").await.expect("stored record");
        assert_eq!(record.benchmark, Some(benchmark));
    }

    #[tokio::test]
    async fn launch_start_process_records_process_start_time() {
        let store = Arc::new(SessionStore::new());
        let record = test_record("process-start-time");
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        command.arg("--help");

        let before = now_ms();
        let launched = store
            .start_process(record, command)
            .await
            .expect("spawn test process");
        let after = now_ms();

        let process_started_at_ms = launched
            .process_started_at_ms
            .expect("process start timestamp");
        assert!(process_started_at_ms >= before);
        assert!(process_started_at_ms <= after);
        assert_eq!(launched.boot_completed_at_ms, None);
        assert_eq!(launched.boot_duration_ms, None);
        let priority = launched.priority.as_ref().expect("priority evidence");
        assert_eq!(priority.start_error, None);
        assert_eq!(priority.promotion, None);
        #[cfg(windows)]
        assert_eq!(priority.start_mode, "below_normal_until_boot");
        #[cfg(not(windows))]
        assert_eq!(priority.start_mode, "noop");

        let stored = store
            .get("process-start-time")
            .await
            .expect("stored record");
        assert_eq!(stored.process_started_at_ms, Some(process_started_at_ms));
        assert_eq!(stored.priority, launched.priority);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_process_exit_preserves_observed_failure_class_without_raw_detail() {
        let store = Arc::new(SessionStore::new());
        let session_id = "class-only-observed-failure";
        store.insert(test_record(session_id)).await;
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "printf '%s\\n' \"Unrecognized VM option '-XX:+UseZGC' /home/alice/.croopor/secret\" >&2; sleep 0.2; exit 1",
        );

        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn failing process");

        let mut terminal_status = None;
        for _ in 0..8 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
                .await
                .expect("status event")
                .expect("broadcast event");
            if let LaunchEvent::Status(status) = event
                && status.state == "exited"
            {
                terminal_status = Some(status);
                break;
            }
        }

        let status = terminal_status.expect("terminal status");
        assert_eq!(
            status.failure_class.as_deref(),
            Some(LaunchFailureClass::JvmUnsupportedOption.as_str())
        );
        assert_eq!(status.failure_detail, None);
        assert_eq!(
            store.observed_failure(session_id).await,
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );

        let stored = store.get(session_id).await.expect("stored record");
        let failure = stored.failure.expect("stored failure");
        assert_eq!(failure.class, LaunchFailureClass::JvmUnsupportedOption);
        assert_eq!(failure.detail, None);
        assert_eq!(
            stored.outcome.expect("stored outcome").reason,
            LaunchSessionExitReason::StartupFailed
        );
    }

    #[tokio::test]
    async fn launch_boot_marker_records_completion_and_duration() {
        let store = SessionStore::new();
        let mut record = test_record("boot-marker-duration");
        record.state = LaunchState::Starting;
        record.process_started_at_ms = Some(now_ms().saturating_sub(4_200));
        store.insert(record).await;

        let mut receiver = store
            .subscribe("boot-marker-duration")
            .await
            .expect("subscribe");
        store
            .emit_log(
                "boot-marker-duration",
                "stdout",
                "[Render thread/INFO]: LWJGL Version: 3.3.3",
            )
            .await;

        let emitted = receiver.recv().await.expect("status event");
        let LaunchEvent::Status(status) = emitted else {
            panic!("expected status event");
        };
        assert_eq!(status.state, "running");

        let stored = store
            .get("boot-marker-duration")
            .await
            .expect("stored record");
        assert_eq!(stored.state, LaunchState::Running);
        assert!(stored.boot_completed_at_ms.is_some());
        assert!(stored.boot_duration_ms.expect("boot duration") >= 4_200);
        let priority = stored.priority.expect("priority evidence");
        assert_eq!(priority.start_error, None);
        assert_eq!(priority.promotion_error, None);
        #[cfg(windows)]
        {
            assert_eq!(priority.start_mode, "below_normal_until_boot");
            assert!(matches!(
                priority.promotion.as_deref(),
                Some("promoted" | "missing_pid" | "failed")
            ));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(priority.start_mode, "noop");
            assert_eq!(priority.promotion.as_deref(), Some("noop"));
        }
    }

    #[tokio::test]
    async fn launch_external_close_after_boot_is_classified_cleanly() {
        let store = SessionStore::new();
        let session_id = "external-close-after-boot";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        record.process_started_at_ms = Some(now_ms().saturating_sub(1_000));
        store.insert(record).await;

        store
            .emit_log(session_id, "stderr", "Unrecognized VM option '-XX:+UseZGC'")
            .await;
        assert_eq!(
            store.observed_failure(session_id).await,
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );

        store
            .emit_log(
                session_id,
                "stdout",
                "[Render thread/INFO]: LWJGL Version: 3.3.3",
            )
            .await;
        assert_eq!(store.observed_failure(session_id).await, None);

        store
            .emit_status(
                session_id,
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
                },
            )
            .await;

        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(stored.failure, None);
        assert_eq!(
            stored.outcome.expect("stored outcome").reason,
            LaunchSessionExitReason::ExternalUserClosed
        );
    }

    #[tokio::test]
    async fn launch_terminal_statuses_emit_distinct_backend_outcomes_and_notices() {
        let store = SessionStore::new();

        let (preboot_status, preboot_record) = insert_and_emit_terminal_status(
            &store,
            terminal_record("preboot-crash", LaunchState::Starting, false),
            terminal_status(Some(1), None, None),
        )
        .await;
        assert_eq!(
            preboot_record.outcome.expect("preboot outcome").reason,
            LaunchSessionExitReason::CrashedBeforeBoot
        );
        assert_eq!(
            preboot_status
                .notice
                .as_ref()
                .map(|notice| notice.message.as_str()),
            Some("Minecraft exited before startup completed.")
        );
        assert_eq!(
            preboot_status.notice.as_ref().map(|notice| notice.tone),
            Some(croopor_launcher::LaunchNoticeTone::Error)
        );
        assert_eq!(
            preboot_status
                .stages
                .last()
                .and_then(|stage| stage.result.as_deref()),
            Some("failed")
        );

        let (stalled_status, stalled_record) = insert_and_emit_terminal_status(
            &store,
            terminal_record("startup-stalled", LaunchState::Monitoring, false),
            terminal_status(
                Some(-1),
                Some("startup_stalled"),
                Some("no startup activity observed"),
            ),
        )
        .await;
        assert_eq!(
            stalled_record.outcome.expect("stalled outcome").reason,
            LaunchSessionExitReason::StartupStalled
        );
        assert_eq!(
            stalled_status
                .notice
                .as_ref()
                .map(|notice| notice.message.as_str()),
            Some("Minecraft did not finish startup in time.")
        );
        assert_eq!(
            stalled_status
                .notice
                .as_ref()
                .and_then(|notice| notice.detail.as_deref()),
            Some("no startup activity observed.")
        );

        let (external_status, external_record) = insert_and_emit_terminal_status(
            &store,
            terminal_record("external-clean-close", LaunchState::Running, true),
            terminal_status(Some(0), None, None),
        )
        .await;
        assert_eq!(
            external_record.outcome.expect("external outcome").reason,
            LaunchSessionExitReason::ExternalUserClosed
        );
        assert_eq!(external_record.failure, None);
        assert_eq!(external_status.notice, None);
        let external_json = serde_json::to_string(&external_status).expect("external status json");
        assert!(!external_json.to_ascii_lowercase().contains("crash"));

        let launcher_stop_session = "launcher-stop";
        let launcher_stop_record =
            terminal_record(launcher_stop_session, LaunchState::Running, true);
        store.insert(launcher_stop_record).await;
        mark_stop_requested(&store, launcher_stop_session).await;
        let mut receiver = store
            .subscribe(launcher_stop_session)
            .await
            .expect("launcher stop subscription");
        store
            .emit_status(launcher_stop_session, terminal_status(Some(-9), None, None))
            .await;
        let launcher_stop_status = recv_status(&mut receiver).await;
        let launcher_stop_record = store
            .get(launcher_stop_session)
            .await
            .expect("launcher stop record");
        assert_eq!(
            launcher_stop_record
                .outcome
                .expect("launcher stop outcome")
                .reason,
            LaunchSessionExitReason::LauncherStopped
        );
        assert_eq!(launcher_stop_status.notice, None);
        assert_eq!(
            launcher_stop_status
                .stages
                .last()
                .and_then(|stage| stage.result.as_deref()),
            Some("exited")
        );

        let (postboot_status, postboot_record) = insert_and_emit_terminal_status(
            &store,
            terminal_record("postboot-crash", LaunchState::Running, true),
            terminal_status(Some(1), None, None),
        )
        .await;
        assert_eq!(
            postboot_record.outcome.expect("postboot outcome").reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            postboot_status
                .notice
                .as_ref()
                .map(|notice| notice.message.as_str()),
            Some("Minecraft crashed after startup.")
        );
        assert_eq!(
            postboot_status
                .stages
                .last()
                .and_then(|stage| stage.result.as_deref()),
            Some("failed")
        );

        let (unknown_status, unknown_record) = insert_and_emit_terminal_status(
            &store,
            terminal_record("unknown-exit", LaunchState::Starting, false),
            terminal_status(Some(0), None, None),
        )
        .await;
        assert_eq!(
            unknown_record.outcome.expect("unknown outcome").reason,
            LaunchSessionExitReason::UnknownExit
        );
        assert_eq!(
            unknown_status
                .notice
                .as_ref()
                .map(|notice| notice.message.as_str()),
            Some("Minecraft exited and the launcher could not classify the reason.")
        );
        assert_eq!(
            unknown_status.notice.as_ref().map(|notice| notice.tone),
            Some(croopor_launcher::LaunchNoticeTone::Error)
        );
    }

    #[tokio::test]
    async fn launch_running_status_without_boot_marker_does_not_record_boot_duration() {
        let store = SessionStore::new();
        let mut record = test_record("timeout-running");
        record.state = LaunchState::Monitoring;
        record.process_started_at_ms = Some(now_ms().saturating_sub(5_000));
        store.insert(record).await;

        store
            .emit_log("timeout-running", "stdout", "ordinary startup output")
            .await;
        store
            .emit_status(
                "timeout-running",
                LaunchStatusEvent {
                    state: "running".to_string(),
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
                },
            )
            .await;

        let stored = store.get("timeout-running").await.expect("stored record");
        assert_eq!(stored.state, LaunchState::Running);
        assert_eq!(stored.boot_completed_at_ms, None);
        assert_eq!(stored.boot_duration_ms, None);
    }

    #[test]
    fn launch_command_xmx_parser_uses_last_megabyte_allocation() {
        assert_eq!(
            command_xmx_mb(&[
                "java".to_string(),
                "-Xmx2048M".to_string(),
                "-Xms512M".to_string(),
                "-Xmx3072M".to_string(),
            ]),
            Some(3072)
        );
        assert_eq!(
            command_xmx_mb(&["java".to_string(), "-Xmx4G".to_string()]),
            None
        );
    }

    #[tokio::test]
    async fn launch_active_memory_allocation_sums_known_non_terminal_sessions() {
        let store = SessionStore::new();
        let mut active = test_record("active-memory");
        active.command = vec!["java".to_string(), "-Xmx2048M".to_string()];
        store.insert(active).await;

        let mut queued_without_command = test_record("queued-without-command");
        queued_without_command.command = Vec::new();
        store.insert(queued_without_command).await;

        let mut exited = test_record("exited-memory");
        exited.state = LaunchState::Exited;
        exited.command = vec!["java".to_string(), "-Xmx8192M".to_string()];
        store.insert(exited).await;

        assert_eq!(store.active_memory_allocation_mb().await, 2048);
    }

    #[tokio::test]
    async fn launch_active_session_count_excludes_terminal_sessions() {
        let store = SessionStore::new();
        store.insert(test_record("queued-count")).await;

        let mut starting = test_record("starting-count");
        starting.state = LaunchState::Starting;
        store.insert(starting).await;

        let mut exited = test_record("exited-count");
        exited.state = LaunchState::Exited;
        store.insert(exited).await;

        assert_eq!(store.active_session_count().await, 2);
    }

    #[tokio::test]
    async fn launch_active_session_id_lookup_ignores_missing_and_terminal_sessions() {
        let store = SessionStore::new();
        let mut failed = test_record("failed-session");
        failed.state = LaunchState::Failed;
        store.insert(failed).await;

        let mut exited = test_record("exited-session");
        exited.state = LaunchState::Exited;
        store.insert(exited).await;

        assert!(
            !store
                .has_any_active_session_id(["missing-session", "failed-session", "exited-session"])
                .await
        );
    }

    #[tokio::test]
    async fn launch_active_session_id_lookup_detects_non_terminal_sessions() {
        let store = SessionStore::new();
        store.insert(test_record("queued-session")).await;

        assert!(
            store
                .has_any_active_session_id(["missing-session", "queued-session"])
                .await
        );
    }

    async fn insert_and_emit_terminal_status(
        store: &SessionStore,
        record: LaunchSessionRecord,
        status: LaunchStatusEvent,
    ) -> (LaunchStatusEvent, LaunchSessionRecord) {
        let session_id = record.session_id.0.clone();
        store.insert(record).await;
        let mut receiver = store.subscribe(&session_id).await.expect("subscribe");
        store.emit_status(&session_id, status).await;
        let status = recv_status(&mut receiver).await;
        let record = store.get(&session_id).await.expect("stored record");
        (status, record)
    }

    async fn recv_status(
        receiver: &mut tokio::sync::broadcast::Receiver<LaunchEvent>,
    ) -> LaunchStatusEvent {
        match receiver.recv().await.expect("status event") {
            LaunchEvent::Status(status) => status,
            other => panic!("expected status event, got {other:?}"),
        }
    }

    async fn mark_stop_requested(store: &SessionStore, session_id: &str) {
        let mut sessions = store.sessions.write().await;
        let entry = sessions
            .get_mut(session_id)
            .expect("session should exist for stop intent");
        entry.stop_requested = true;
    }

    fn terminal_record(
        session_id: &str,
        state: LaunchState,
        boot_completed: bool,
    ) -> LaunchSessionRecord {
        let mut record = test_record(session_id);
        record.state = state;
        record.process_started_at_ms = Some(now_ms().saturating_sub(1_000));
        if boot_completed {
            record.boot_completed_at_ms = Some(now_ms().saturating_sub(100));
            record.boot_duration_ms = Some(900);
        }
        record
    }

    fn terminal_status(
        exit_code: Option<i32>,
        failure_class: Option<&str>,
        failure_detail: Option<&str>,
    ) -> LaunchStatusEvent {
        LaunchStatusEvent {
            state: "exited".to_string(),
            benchmark: None,
            pid: None,
            exit_code,
            failure_class: failure_class.map(ToOwned::to_owned),
            failure_detail: failure_detail.map(ToOwned::to_owned),
            healing: None,
            guardian: None,
            outcome: None,
            notice: None,
            evidence: Vec::new(),
            stages: Vec::new(),
        }
    }

    fn assert_public_session_payload_excludes_sensitive_content(data: &str) {
        for fragment in [
            "/home/alice",
            "/home/",
            r"C:\Users",
            "AppData",
            "java.exe",
            "--accessToken",
            "-Xmx8192M",
            "-Dtoken",
            "raw-secret",
            "provider_payload",
            "account_id",
            "account-secret",
            "username",
            "SecretPlayer",
            "token\":\"secret",
        ] {
            assert!(
                !data.contains(fragment),
                "public session payload leaked fragment {fragment:?}: {data}"
            );
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
}
