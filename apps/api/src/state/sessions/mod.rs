mod classify;
mod supervisor;

use croopor_launcher::{
    LaunchEvent, LaunchFailure, LaunchFailureClass, LaunchLogEvent, LaunchSessionRecord,
    LaunchStageRecord, LaunchState, LaunchStatusEvent, launch_stage_label, launch_state_name,
};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, broadcast};

struct SessionEntry {
    record: LaunchSessionRecord,
    events: broadcast::Sender<LaunchEvent>,
    child: Option<Arc<Mutex<Child>>>,
    startup_observed: Arc<AtomicBool>,
    boot_completed: Arc<AtomicBool>,
    log_count: Arc<AtomicUsize>,
    observed_failure: Option<LaunchFailure>,
}

pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionEntry>>,
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
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
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
            },
        );
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
        Some(entry.record.clone())
    }

    pub async fn subscribe(&self, session_id: &str) -> Option<broadcast::Receiver<LaunchEvent>> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.events.subscribe())
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
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.startup_observed.store(true, Ordering::Relaxed);
            entry.log_count.fetch_add(1, Ordering::Relaxed);
            if classify::boot_marker_detected(&text) {
                record_boot_completion(entry, now_ms());
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
                    stages: Vec::new(),
                };
                apply_status_update(entry, &mut status);
                let _ = entry.events.send(LaunchEvent::Status(status));
            }
            let failure_class = classify::classify_failure_text(&text);
            if failure_class != LaunchFailureClass::Unknown {
                entry.observed_failure = Some(LaunchFailure {
                    class: failure_class,
                    detail: Some(text.clone()),
                });
            }
            let _ = entry
                .events
                .send(LaunchEvent::Log(LaunchLogEvent { source, text }));
        }
    }

    pub async fn emit_status(&self, session_id: &str, mut event: LaunchStatusEvent) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            apply_status_update(entry, &mut event);
            let _ = entry.events.send(LaunchEvent::Status(event));
        }
    }

    pub async fn start_process(
        self: &Arc<Self>,
        mut record: LaunchSessionRecord,
        mut command: Command,
    ) -> std::io::Result<LaunchSessionRecord> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
                    },
                );
            }
        }

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

    pub async fn kill(&self, session_id: &str) -> std::io::Result<()> {
        let child = self
            .sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.child.clone());
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
    }

    pub async fn observed_failure(&self, session_id: &str) -> Option<LaunchFailure> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.observed_failure.clone())
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
    update_stage_history(&mut entry.record, event, now);

    entry.record.state = classify::parse_launch_state(&event.state);
    if event.pid.is_some() {
        entry.record.pid = event.pid;
    }
    if event.exit_code.is_some() {
        entry.record.exit_code = event.exit_code;
    }
    if let Some(failure_class) = &event.failure_class {
        entry.record.failure = Some(LaunchFailure {
            class: classify::parse_failure_class(failure_class),
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
    event.stages = entry.record.stages.clone();
}

fn record_boot_completion(entry: &mut SessionEntry, now: u64) {
    if entry.boot_completed.swap(true, Ordering::Relaxed) {
        return;
    }
    entry.record.boot_completed_at_ms = Some(now);
    entry.record.boot_duration_ms = entry
        .record
        .process_started_at_ms
        .map(|started_at| now.saturating_sub(started_at));
}

fn ensure_stage_started(record: &mut LaunchSessionRecord, now: u64) {
    if record.stages.is_empty() {
        let stage = launch_state_name(record.state).to_string();
        record.stages.push(start_stage(&stage, now, None, None));
    }
}

fn update_stage_history(record: &mut LaunchSessionRecord, event: &LaunchStatusEvent, now: u64) {
    ensure_stage_started(record, now);

    let next_stage = event.state.as_str();
    let terminal_result = terminal_stage_result(event);
    let (warnings, fallback_reason) = stage_notes(event);
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
        if let Some(result) = terminal_result {
            close_open_stage(record.stages.last_mut(), now, result);
        }
        return;
    }

    let previous_result = terminal_result.unwrap_or("ok");
    close_open_stage(record.stages.last_mut(), now, previous_result);
    let mut next = start_stage(next_stage, now, Some(warnings), fallback_reason);
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
        "exited" if event.failure_class.is_some() => Some("failed"),
        "exited" => Some("exited"),
        _ => None,
    }
}

fn stage_notes(event: &LaunchStatusEvent) -> (Vec<String>, Option<String>) {
    let Some(healing) = event.healing.as_ref().and_then(|value| value.as_object()) else {
        return (Vec::new(), None);
    };
    let warnings = healing
        .get("warnings")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let fallback_reason = healing
        .get("fallback_applied")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    (warnings, fallback_reason)
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
        if !stage.warnings.iter().any(|existing| existing == warning) {
            stage.warnings.push(warning.clone());
        }
    }
    if stage.fallback_reason.is_none()
        && let Some(fallback_reason) = fallback_reason
    {
        stage.fallback_reason = Some(fallback_reason.to_string());
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
    use croopor_launcher::SessionId;
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

        let stored = store
            .get("process-start-time")
            .await
            .expect("stored record");
        assert_eq!(stored.process_started_at_ms, Some(process_started_at_ms));
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
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            healing: None,
            guardian: None,
            stages: Vec::new(),
        }
    }
}
