//! Session incarnations own their process, output, and preparation capabilities.
//! Accepted work survives loss of the HTTP waiter. Only the launch coordinator
//! can supply a `PreparedSession`; this module has no public command constructor.

use super::{
    logs::{LogCollector, LogEntry, LogStream, MAX_OUTPUT_CHUNK_BYTES},
    outcome::{SessionExitFacts, SessionOutcome, classify_session_outcome},
    prepare::PreparedSession,
    process::{OwnedProcess, ProcessOutput, SpawnError},
    reports::{LaunchProofRecord, LaunchProofScenario, LaunchReportStore, SessionReportInput},
};
use crate::{
    instances::{directory::RegisteredInstance, model::InstanceId},
    tasks::{CancellationToken, TaskOwner},
    telemetry::{
        Telemetry, TelemetryErrorKind, TelemetryEvent, TelemetryLaunchOutcome, TelemetryLoader,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{broadcast, watch},
};

const PROCESS_POLL: Duration = Duration::from_millis(100);
const SETTLEMENT_DEADLINE: Duration = Duration::from_secs(5);
const RETAINED_TERMINAL_SESSIONS: usize = 64;
const MAX_SESSION_IDENTITIES: usize = 4096;
const STARTUP_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

/// One accepted attempt spans preparation and its eventual process. Completion
/// measures startup, not game exit; later stops/crashes cannot duplicate it.
pub(super) struct LaunchAttemptTelemetry {
    telemetry: Option<Arc<Telemetry>>,
    completed: AtomicBool,
}

impl LaunchAttemptTelemetry {
    pub(super) fn started(telemetry: Option<Arc<Telemetry>>, loader: &str) -> Arc<Self> {
        if let Some(telemetry) = &telemetry {
            telemetry.emit(TelemetryEvent::LaunchStarted {
                loader: TelemetryLoader::from_key(loader),
            });
        }
        Arc::new(Self {
            telemetry,
            completed: AtomicBool::new(false),
        })
    }

    fn complete(&self, outcome: TelemetryLaunchOutcome, error: Option<TelemetryErrorKind>) {
        if self.completed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(telemetry) = &self.telemetry {
            telemetry.emit(TelemetryEvent::LaunchCompleted { outcome });
            if let Some(kind) = error {
                telemetry.emit(TelemetryEvent::ErrorCaptured { kind });
            }
        }
    }

    pub(super) fn failure(&self, error: Option<TelemetryErrorKind>) {
        self.complete(TelemetryLaunchOutcome::Failure, error);
    }

    fn observe_startup(&self, boot: bool, has_output: bool, elapsed: Duration) -> bool {
        // The retained runner accepts a boot marker immediately, or a live
        // process with ordinary output after its observation window.
        if boot || has_output && elapsed >= STARTUP_OBSERVATION_TIMEOUT {
            self.complete(TelemetryLaunchOutcome::Success, None);
            return true;
        }
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Starting,
    Running,
    Stopping,
    Settling,
    Unresolved,
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionViewModel {
    pub state_id: String,
    pub label: String,
    pub progress_pct: u8,
    pub terminal: bool,
    pub playing: bool,
    pub process_live: bool,
    pub can_stop: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionNotice {
    pub message: String,
    pub tone: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub instance_id: InstanceId,
    pub revision: u64,
    pub phase: SessionPhase,
    pub launched_at: String,
    pub started_at_ms: Option<u64>,
    pub pid: Option<u32>,
    /// True while any owned process may still execute, including descendants.
    pub process_alive: bool,
    pub stop_allowed: bool,
    pub exit_code: Option<i32>,
    pub tree_settled: bool,
    pub output_drained: bool,
    pub boot_observed: bool,
    pub outcome: Option<SessionOutcome>,
    pub notice: Option<SessionNotice>,
    pub view_model: SessionViewModel,
}

impl SessionSnapshot {
    fn starting(instance_id: InstanceId, session_id: String) -> Self {
        let mut snapshot = Self {
            session_id,
            instance_id,
            revision: 1,
            launched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            phase: SessionPhase::Starting,
            started_at_ms: None,
            pid: None,
            process_alive: false,
            stop_allowed: true,
            exit_code: None,
            tree_settled: false,
            output_drained: false,
            boot_observed: false,
            outcome: None,
            notice: None,
            view_model: SessionViewModel {
                state_id: String::new(),
                label: String::new(),
                progress_pct: 0,
                terminal: false,
                playing: false,
                process_live: false,
                can_stop: true,
            },
        };
        snapshot.project();
        snapshot
    }

    /// The coordinator must match this terminal proof to its durable accepted
    /// intent before restoring a historical session without a live owner.
    pub(super) fn from_report(report: &LaunchProofRecord, instance_id: InstanceId) -> Self {
        let mut snapshot = Self::starting(instance_id, report.session_id.clone());
        snapshot.phase = SessionPhase::Exited;
        snapshot.launched_at = report.launched_at.clone();
        snapshot.stop_allowed = false;
        snapshot.exit_code = report.exit_code;
        snapshot.tree_settled = true;
        snapshot.output_drained = true;
        snapshot.boot_observed = report.boot_duration_ms.is_some();
        snapshot.outcome = Some(report.session_outcome.clone());
        snapshot.project();
        snapshot
    }

    fn report_unavailable(&mut self) {
        self.notice = Some(SessionNotice {
            message:
                "The session ended and cleanup is complete, but its launch report is unavailable."
                    .into(),
            tone: "warned".into(),
        });
    }

    fn project(&mut self) {
        self.notice = if self.phase == SessionPhase::Unresolved {
            Some(SessionNotice {
                message: "Session cleanup is unfinished. The instance remains in use.".to_owned(),
                tone: "warned".to_owned(),
            })
        } else if self.phase == SessionPhase::Exited {
            self.outcome.as_ref().map(|outcome| SessionNotice {
                message: outcome.summary().to_owned(),
                tone: if outcome.kind == super::outcome::SessionOutcomeKind::Failed {
                    "error"
                } else {
                    "info"
                }
                .to_owned(),
            })
        } else {
            None
        };
        let (state, label, progress) = match self.phase {
            SessionPhase::Starting if self.process_alive => ("starting", "Starting Minecraft", 65),
            SessionPhase::Starting => ("starting", "Starting Minecraft", 25),
            SessionPhase::Running => ("running", "Playing", 100),
            SessionPhase::Stopping => ("stopping", "Stopping Minecraft", 100),
            SessionPhase::Settling => ("stopping", "Finishing session", 100),
            SessionPhase::Unresolved => ("unresolved", "Session cleanup needs attention", 100),
            SessionPhase::Exited => ("exited", "Session ended", 100),
        };
        self.view_model = SessionViewModel {
            state_id: state.to_owned(),
            label: label.to_owned(),
            progress_pct: progress,
            terminal: self.phase == SessionPhase::Exited,
            playing: self.phase == SessionPhase::Running && self.process_alive,
            process_live: self.process_alive,
            can_stop: self.stop_allowed,
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("A game session already owns this instance.")]
    Busy,
    #[error("The game session is not available.")]
    NotFound,
    #[error("New game sessions are unavailable while the application closes.")]
    Closing,
    #[error("The application cannot accept another game session right now.")]
    AtCapacity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCommandInspection {
    pub session_id: String,
    pub command_arg_count: usize,
    pub java_path_present: bool,
}

/// Subscribe before reading this retained history. Broadcast lag requires a
/// fresh subscription; callers never recover a lost event by repeating launch.
pub struct SessionLogSubscription {
    pub entries: Vec<LogEntry>,
    pub events: broadcast::Receiver<LogEntry>,
}

struct EntryState {
    snapshot: SessionSnapshot,
    logs: LogCollector,
    report: Option<LaunchProofRecord>,
    process_started: Option<tokio::time::Instant>,
    boot_duration_ms: Option<u64>,
    observation: Option<ObservedSettlement>,
    report_unavailable: bool,
}

struct SessionEntry {
    state: Mutex<EntryState>,
    snapshot: watch::Sender<SessionSnapshot>,
    log_events: broadcast::Sender<LogEntry>,
    stop: watch::Sender<bool>,
    changes: watch::Sender<u64>,
    command_inspection: SessionCommandInspection,
    reports: Option<LaunchReportStore>,
    version_id: String,
    scenario: LaunchProofScenario,
    telemetry: Arc<LaunchAttemptTelemetry>,
    acceptance: Option<super::coordinator::AcceptedIntent>,
}

/// Minted only after native cleanup and either verified child/tree/output
/// settlement or the branch which never created a child. It is not a report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ObservedSettlement {
    version_id: String,
    launched_at: String,
    ended_at: String,
    process: SettledProcess,
    boot_observed: bool,
    failure_classes: Vec<super::outcome::FailureClass>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SettledProcess {
    NoChild {
        stopped: bool,
    },
    ChildExited {
        exit_code: Option<i32>,
        signal: Option<i32>,
        stop_requested: bool,
        was_running: bool,
        spawn_failed: bool,
    },
}

impl ObservedSettlement {
    pub(super) fn validate(&self) -> bool {
        let time = |value: &str| {
            value.len() == 24
                && chrono::DateTime::parse_from_rfc3339(value).is_ok_and(|time| {
                    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) == value
                })
        };
        axial_minecraft::portable_path::PortableFileName::new_exact(&format!(
            "{}.json",
            self.version_id
        ))
        .is_ok()
            && axial_minecraft::portable_path::PortableFileName::new_exact(&self.version_id).is_ok()
            && (!self.version_id.starts_with("loader-v2-")
                || axial_minecraft::loaders::api::is_canonical_installed_loader_id(
                    &self.version_id,
                ))
            && time(&self.launched_at)
            && time(&self.ended_at)
            && self.failure_classes.len() <= 17
            && !self
                .failure_classes
                .iter()
                .enumerate()
                .any(|(index, class)| {
                    *class == super::outcome::FailureClass::Unknown
                        || self.failure_classes[..index].contains(class)
                })
            && match self.process {
                SettledProcess::NoChild { .. } => {
                    !self.boot_observed && self.failure_classes.is_empty()
                }
                SettledProcess::ChildExited { signal, .. } => signal.is_none_or(|value| value > 0),
            }
    }

    fn exit_code(&self) -> Option<i32> {
        match self.process {
            SettledProcess::NoChild { .. } => None,
            SettledProcess::ChildExited { exit_code, .. } => exit_code,
        }
    }

    fn outcome(&self) -> SessionOutcome {
        let (exit_code, signal, stop_requested, was_running) = match self.process {
            SettledProcess::NoChild { stopped: false }
            | SettledProcess::ChildExited {
                spawn_failed: true, ..
            } => {
                return SessionOutcome::spawn_failed();
            }
            SettledProcess::NoChild { stopped: true } => (None, None, true, false),
            SettledProcess::ChildExited {
                exit_code,
                signal,
                stop_requested,
                was_running,
                ..
            } => (exit_code, signal, stop_requested, was_running),
        };
        classify_session_outcome(
            SessionExitFacts {
                process_exited: true,
                exit_code,
                signal,
                stop_requested,
                was_running,
                tree_settled: true,
                outputs_drained: true,
            },
            &super::outcome::LaunchEvidence {
                boot_observed: self.boot_observed,
                failure_classes: self.failure_classes.clone(),
            },
        )
        .expect("settled process facts")
    }

    pub(super) fn snapshot(&self, session_id: String, instance_id: InstanceId) -> SessionSnapshot {
        let mut snapshot = SessionSnapshot::starting(instance_id, session_id);
        snapshot.launched_at = self.launched_at.clone();
        snapshot.phase = SessionPhase::Exited;
        snapshot.stop_allowed = false;
        snapshot.exit_code = self.exit_code();
        snapshot.tree_settled = true;
        snapshot.output_drained = true;
        snapshot.boot_observed = self.boot_observed;
        snapshot.outcome = Some(self.outcome());
        snapshot.project();
        snapshot.report_unavailable();
        snapshot
    }

    pub(super) fn matches_report(&self, report: &LaunchProofRecord) -> bool {
        self.version_id == report.version_id
            && self.launched_at == report.launched_at
            && self.ended_at == report.recorded_at
            && self.exit_code() == report.exit_code
            && self.boot_observed == report.boot_duration_ms.is_some()
            && self.outcome() == report.session_outcome
    }

    pub(super) fn matches_requested_version(&self, requested: Option<&str>) -> bool {
        requested.is_none_or(|version| version == self.version_id)
    }
}

impl SessionEntry {
    /// Capture once, before any fallible report projection. Failed observation
    /// publication retains this owner; a later report failure does not.
    fn persist_settlement(
        &self,
        process: SettledProcess,
        startup_stalled: bool,
    ) -> Option<SessionOutcome> {
        let (observation, first_attempt) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let first_attempt = state.observation.is_none();
            if state.observation.is_none() {
                let mut evidence = state.logs.evidence().clone();
                if startup_stalled {
                    evidence.observe_failure(super::outcome::FailureClass::StartupStalled);
                }
                state.observation = Some(ObservedSettlement {
                    version_id: self.version_id.clone(),
                    launched_at: state.snapshot.launched_at.clone(),
                    ended_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    process,
                    boot_observed: evidence.boot_observed,
                    failure_classes: evidence.failure_classes,
                });
            }
            (
                state.observation.clone().expect("captured settlement"),
                first_attempt,
            )
        };
        let durable = if let Some(acceptance) = &self.acceptance {
            match acceptance.observe(&observation) {
                Ok(durable) => durable,
                Err(error) => {
                    if first_attempt {
                        tracing::warn!(code = ?error, "Verified session settlement could not be persisted.");
                    }
                    return None;
                }
            }
        } else {
            false
        };
        let outcome = observation.outcome();
        if !self.persist(&outcome) {
            if !durable {
                return None;
            }
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .report_unavailable = true;
        }
        Some(outcome)
    }

    fn persist(&self, outcome: &SessionOutcome) -> bool {
        let Some(reports) = &self.reports else {
            return true;
        };
        let (report, first_attempt) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let first_attempt = state.report.is_none();
            if first_attempt {
                let mut report = LaunchProofRecord::from_session(SessionReportInput {
                    session_id: state.snapshot.session_id.clone(),
                    instance_id: state.snapshot.instance_id.to_string(),
                    version_id: self.version_id.clone(),
                    launched_at: state.snapshot.launched_at.clone(),
                    ended_at: state
                        .observation
                        .as_ref()
                        .expect("observed settlement")
                        .ended_at
                        .clone(),
                    outcome: outcome.clone(),
                    entries: state.logs.entries(),
                    exit_code: state.snapshot.exit_code,
                    boot_duration_ms: state.boot_duration_ms,
                    logs_dropped: state.logs.dropped_entries(),
                });
                report.scenario = self.scenario.clone();
                state.report = Some(report);
            }
            (
                state.report.clone().expect("captured report"),
                first_attempt,
            )
        };
        match reports.record(report, &super::logs::Redactor::new(Vec::new())) {
            Ok(()) => true,
            Err(error) => {
                if first_attempt {
                    let reason = match error {
                        super::reports::ReportError::Invalid => "invalid",
                        super::reports::ReportError::TooLarge => "too_large",
                        super::reports::ReportError::ConflictingSession => "conflict",
                        super::reports::ReportError::Storage(_) => "storage",
                    };
                    tracing::warn!(reason, "Terminal launch report could not be persisted.");
                }
                false
            }
        }
    }

    fn publish(&self, update: impl FnOnce(&mut SessionSnapshot)) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let before = state.snapshot.clone();
        update(&mut state.snapshot);
        state.snapshot.project();
        if state.report_unavailable && state.snapshot.phase == SessionPhase::Exited {
            state.snapshot.report_unavailable();
        }
        if state.snapshot != before {
            state.snapshot.revision = state
                .snapshot
                .revision
                .checked_add(1)
                .expect("session revision exhausted");
            self.snapshot.send_replace(state.snapshot.clone());
            self.changes
                .send_modify(|revision| *revision = revision.saturating_add(1));
        }
    }

    fn output(&self, stream: LogStream, bytes: Option<&[u8]>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let entries = match bytes {
            Some(bytes) => state.logs.push(stream, bytes),
            None => state.logs.finish_stream(stream),
        };
        for entry in entries {
            let _ = self.log_events.send(entry);
        }
        let boot = state.logs.evidence().boot_observed;
        if boot && state.boot_duration_ms.is_none() {
            state.boot_duration_ms = state
                .process_started
                .map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
        let drained = state.logs.outputs_drained();
        drop(state);
        self.publish(|snapshot| {
            snapshot.boot_observed = boot;
            snapshot.output_drained = drained;
        });
    }

    fn current(&self) -> SessionSnapshot {
        self.snapshot.borrow().clone()
    }

    fn stop(&self) -> SessionSnapshot {
        // Exact entry identity means a late stop can never kill a new incarnation.
        if self.current().phase != SessionPhase::Exited {
            self.stop.send_replace(true);
            self.publish(|snapshot| {
                if matches!(
                    snapshot.phase,
                    SessionPhase::Starting | SessionPhase::Running
                ) {
                    snapshot.phase = SessionPhase::Stopping;
                }
            });
        }
        self.current()
    }
}

struct Registry {
    closing: bool,
    current: BTreeMap<InstanceId, String>,
    sessions: BTreeMap<String, Arc<SessionEntry>>,
    retired: BTreeMap<String, SessionSnapshot>,
    order: VecDeque<String>,
}

#[derive(Clone)]
pub struct SessionManager {
    tasks: TaskOwner,
    registry: Arc<Mutex<Registry>>,
    changes: watch::Sender<u64>,
    reports: Option<LaunchReportStore>,
}

impl SessionManager {
    pub fn new(tasks: TaskOwner) -> Self {
        Self {
            tasks,
            registry: Arc::new(Mutex::new(Registry {
                closing: false,
                current: BTreeMap::new(),
                sessions: BTreeMap::new(),
                retired: BTreeMap::new(),
                order: VecDeque::new(),
            })),
            changes: watch::channel(0).0,
            reports: None,
        }
    }

    pub fn with_reports(tasks: TaskOwner, reports: LaunchReportStore) -> Self {
        Self {
            reports: Some(reports),
            ..Self::new(tasks)
        }
    }

    pub(super) fn start_reserved(
        &self,
        mut prepared: PreparedSession,
        session_id: String,
        acceptance: super::coordinator::AcceptedIntent,
    ) -> Result<SessionSnapshot, SessionError> {
        if !uuid::Uuid::parse_str(&session_id)
            .is_ok_and(|id| !id.is_nil() && id.to_string() == session_id)
        {
            return Err(SessionError::Busy);
        }
        let instance_id = prepared.instance_id().clone();
        if !acceptance.matches_session(&session_id, &instance_id) {
            return Err(SessionError::Busy);
        }
        let snapshot = SessionSnapshot::starting(instance_id.clone(), session_id);
        let entry = Arc::new(SessionEntry {
            state: Mutex::new(EntryState {
                snapshot: snapshot.clone(),
                logs: LogCollector::new(prepared.take_secrets()),
                report: None,
                process_started: None,
                boot_duration_ms: None,
                observation: None,
                report_unavailable: false,
            }),
            snapshot: watch::channel(snapshot.clone()).0,
            log_events: broadcast::channel(256).0,
            stop: watch::channel(false).0,
            changes: self.changes.clone(),
            reports: self.reports.clone(),
            version_id: prepared.version_id().to_owned(),
            scenario: prepared.scenario().clone(),
            telemetry: prepared.telemetry(),
            acceptance: Some(acceptance),
            command_inspection: SessionCommandInspection {
                session_id: snapshot.session_id.clone(),
                command_arg_count: prepared.validated_command().args().len(),
                java_path_present: !prepared
                    .validated_command()
                    .program()
                    .as_os_str()
                    .is_empty(),
            },
        });
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if registry.closing {
            return Err(SessionError::Closing);
        }
        if registry.sessions.contains_key(&snapshot.session_id)
            || registry.retired.contains_key(&snapshot.session_id)
        {
            return Err(SessionError::Busy);
        }
        if registry.sessions.len() + registry.retired.len() >= MAX_SESSION_IDENTITIES {
            return Err(SessionError::AtCapacity);
        }
        if registry
            .current
            .get(&instance_id)
            .and_then(|id| registry.sessions.get(id))
            .is_some_and(|entry| entry.current().phase != SessionPhase::Exited)
        {
            return Err(SessionError::Busy);
        }
        let prepared = Arc::new(prepared);
        let worker_prepared = Arc::clone(&prepared);
        let worker_entry = Arc::clone(&entry);
        // The task owner retains a separate preparation reference across a panic.
        // Dropping this returned handle only drops a waiter, not accepted work.
        self.tasks
            .try_spawn(prepared, move |cancellation| async move {
                run_session(worker_prepared, worker_entry, cancellation).await;
            })
            .map_err(|_| SessionError::AtCapacity)?;
        registry
            .current
            .insert(instance_id, snapshot.session_id.clone());
        registry.sessions.insert(snapshot.session_id.clone(), entry);
        registry.order.push_back(snapshot.session_id.clone());
        evict_terminal_history(&mut registry);
        self.changes
            .send_modify(|revision| *revision = revision.saturating_add(1));
        Ok(snapshot)
    }

    pub fn snapshot(&self, instance_id: &InstanceId) -> Option<SessionSnapshot> {
        self.entry(instance_id).map(|entry| entry.current())
    }

    pub fn snapshot_by_session_id(&self, session_id: &str) -> Option<SessionSnapshot> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        registry
            .sessions
            .get(session_id)
            .map(|entry| entry.current())
            .or_else(|| registry.retired.get(session_id).cloned())
    }

    pub fn snapshots(&self) -> Vec<SessionSnapshot> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        registry
            .current
            .values()
            .filter_map(|id| registry.sessions.get(id))
            .map(|entry| entry.current())
            .collect()
    }

    pub fn sessions(&self) -> Vec<SessionSnapshot> {
        self.snapshots()
    }

    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn subscribe(&self, instance_id: &InstanceId) -> Option<watch::Receiver<SessionSnapshot>> {
        self.entry(instance_id)
            .map(|entry| entry.snapshot.subscribe())
    }

    pub fn subscribe_by_session_id(
        &self,
        session_id: &str,
    ) -> Option<watch::Receiver<SessionSnapshot>> {
        self.entry_by_session_id(session_id)
            .map(|entry| entry.snapshot.subscribe())
    }

    pub fn logs(&self, instance_id: &InstanceId) -> Vec<LogEntry> {
        self.entry(instance_id)
            .map(|entry| {
                entry
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .logs
                    .entries()
            })
            .unwrap_or_default()
    }

    pub fn logs_by_session_id(&self, session_id: &str) -> Option<Vec<LogEntry>> {
        self.entry_by_session_id(session_id)
            .map(|entry| {
                entry
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .logs
                    .entries()
            })
            .or_else(|| {
                self.reports
                    .as_ref()?
                    .get(session_id)
                    .ok()
                    .flatten()
                    .map(|report| report.logs)
            })
    }

    pub fn subscribe_logs_by_session_id(&self, session_id: &str) -> Option<SessionLogSubscription> {
        self.entry_by_session_id(session_id).map(|entry| {
            let state = entry
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let events = entry.log_events.subscribe();
            SessionLogSubscription {
                entries: state.logs.entries(),
                events,
            }
        })
    }

    pub fn command_inspection(&self, session_id: &str) -> Option<SessionCommandInspection> {
        self.entry_by_session_id(session_id)
            .map(|entry| entry.command_inspection.clone())
    }

    pub fn stop(&self, instance_id: &InstanceId) -> Result<SessionSnapshot, SessionError> {
        self.entry(instance_id)
            .map(|entry| entry.stop())
            .ok_or(SessionError::NotFound)
    }

    pub fn stop_by_session_id(&self, session_id: &str) -> Result<SessionSnapshot, SessionError> {
        self.entry_by_session_id(session_id)
            .map(|entry| entry.stop())
            .or_else(|| self.snapshot_by_session_id(session_id))
            .ok_or(SessionError::NotFound)
    }

    pub async fn shutdown(&self, timeout: Duration) -> Result<(), Vec<SessionSnapshot>> {
        let mut changes = self.subscribe_changes();
        let entries = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            registry.closing = true;
            registry.sessions.values().cloned().collect::<Vec<_>>()
        };
        for entry in &entries {
            entry.stop();
        }
        let settled = tokio::time::timeout(timeout, async {
            loop {
                if entries
                    .iter()
                    .all(|entry| entry.current().phase == SessionPhase::Exited)
                {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        let unresolved = entries
            .iter()
            .map(|entry| entry.current())
            .filter(|snapshot| snapshot.phase != SessionPhase::Exited)
            .collect::<Vec<_>>();
        if settled.is_ok() && unresolved.is_empty() {
            Ok(())
        } else {
            Err(unresolved)
        }
    }

    fn entry(&self, instance_id: &InstanceId) -> Option<Arc<SessionEntry>> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        registry
            .current
            .get(instance_id)
            .and_then(|id| registry.sessions.get(id))
            .cloned()
    }

    fn entry_by_session_id(&self, session_id: &str) -> Option<Arc<SessionEntry>> {
        self.registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .get(session_id)
            .cloned()
    }
}

fn evict_terminal_history(registry: &mut Registry) {
    let mut terminal_count = registry
        .sessions
        .values()
        .filter(|entry| entry.current().phase == SessionPhase::Exited)
        .count();
    registry.order.retain(|id| {
        if terminal_count <= RETAINED_TERMINAL_SESSIONS {
            return true;
        }
        let Some(entry) = registry.sessions.get(id) else {
            return false;
        };
        let snapshot = entry.current();
        if snapshot.phase != SessionPhase::Exited {
            return true;
        }
        if registry.current.get(&snapshot.instance_id) == Some(id) {
            registry.current.remove(&snapshot.instance_id);
        }
        registry.sessions.remove(id);
        registry.retired.insert(id.clone(), snapshot);
        terminal_count -= 1;
        false
    });
}

/// On a panic the task owner retains preparation and this sentinel marks the
/// session nonterminal. It never claims a child/tree was reaped by Drop.
struct SettlementSentinel {
    entry: Arc<SessionEntry>,
    settled: bool,
}
impl Drop for SettlementSentinel {
    fn drop(&mut self) {
        if !self.settled {
            self.entry.telemetry.failure(None);
            self.entry.publish(|snapshot| {
                snapshot.phase = SessionPhase::Unresolved;
                snapshot.outcome = None;
            });
        }
    }
}

async fn run_session(
    prepared: Arc<PreparedSession>,
    entry: Arc<SessionEntry>,
    cancellation: CancellationToken,
) {
    let mut sentinel = SettlementSentinel {
        entry: Arc::clone(&entry),
        settled: false,
    };
    let stop = entry.stop.subscribe();
    if *stop.borrow() || cancellation.is_cancelled() {
        finish_without_process(&entry, true, prepared.natives()).await;
        sentinel.settled = true;
        return;
    }
    if prepared.validate_before_spawn().is_err() {
        finish_without_process(&entry, false, prepared.natives()).await;
        sentinel.settled = true;
        return;
    }
    let validated = prepared.validated_command();
    let mut command = Command::new(validated.program());
    command
        .args(validated.args())
        .envs(validated.env())
        .current_dir(validated.cwd());
    for key in [
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
    ] {
        command.env_remove(key);
    }
    let (process, output, spawn_failed) = match OwnedProcess::spawn(command) {
        Ok((process, output)) => (process, output, false),
        Err(SpawnError::BeforeSpawn(_error)) => {
            entry
                .telemetry
                .failure(Some(TelemetryErrorKind::LaunchSpawnFailed));
            finish_without_process(&entry, false, prepared.natives()).await;
            sentinel.settled = true;
            return;
        }
        Err(SpawnError::Unsettled(process, output, _error)) => (process, output, true),
    };
    supervise_process(
        process,
        output,
        spawn_failed,
        entry,
        cancellation,
        stop,
        prepared.natives(),
        Some(prepared.instance().clone()),
    )
    .await;
    sentinel.settled = true;
}

async fn supervise_process(
    mut process: OwnedProcess,
    mut output: ProcessOutput,
    spawn_failed: bool,
    entry: Arc<SessionEntry>,
    cancellation: CancellationToken,
    mut stop: watch::Receiver<bool>,
    natives: Option<Arc<crate::install::vanilla::PreparedNatives>>,
    instance: Option<RegisteredInstance>,
) {
    if spawn_failed {
        entry
            .telemetry
            .failure(Some(TelemetryErrorKind::LaunchSpawnFailed));
    }
    let pid = process.pid();
    entry
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .process_started = Some(tokio::time::Instant::now());
    entry.publish(|snapshot| {
        snapshot.pid = pid;
        snapshot.process_alive = true;
        snapshot.started_at_ms = Some(now_ms());
        snapshot.phase = if spawn_failed {
            SessionPhase::Settling
        } else {
            SessionPhase::Starting
        };
    });
    let mut stdout_buffer = [0_u8; MAX_OUTPUT_CHUNK_BYTES];
    let mut stderr_buffer = [0_u8; MAX_OUTPUT_CHUNK_BYTES];
    let mut stdout_finished = false;
    let mut stderr_finished = false;
    let mut output_failed = false;
    let mut stop_requested = false;
    let mut settlement_failed = false;
    let mut startup_observed = false;
    let mut startup_stalled = false;
    let launched_at = entry.current().launched_at;
    let mut settling_since = spawn_failed.then(tokio::time::Instant::now);
    let mut ticker = tokio::time::interval(PROCESS_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if output_failed {
            settling_since.get_or_insert_with(tokio::time::Instant::now);
        }
        if !startup_stalled && (*stop.borrow() || cancellation.is_cancelled()) {
            stop_requested = true;
            settling_since.get_or_insert_with(tokio::time::Instant::now);
        }
        let exited = process.try_wait();
        let status = match exited {
            Ok(status) => status,
            Err(_) => {
                settling_since.get_or_insert_with(tokio::time::Instant::now);
                None
            }
        };
        if status.is_some() {
            settling_since.get_or_insert_with(tokio::time::Instant::now);
        }
        if !startup_observed && settling_since.is_none() {
            let state = entry
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // The retained runner stalls only a silent attempt at 30 seconds.
            // An observed boot or ordinary output completes startup below.
            if !state.logs.evidence().boot_observed
                && !state.logs.has_entries()
                && state
                    .process_started
                    .is_some_and(|started| started.elapsed() >= STARTUP_OBSERVATION_TIMEOUT)
                && !*stop.borrow()
                && !cancellation.is_cancelled()
            {
                startup_stalled = true;
                settling_since = Some(tokio::time::Instant::now());
            }
        }
        let mut tree_settled = false;
        if settling_since.is_some() {
            let _ = process.terminate();
            tree_settled = process.tree_settled().unwrap_or(false);
        }
        let drained = stdout_finished && stderr_finished && !output_failed;
        let observed_now =
            if !startup_observed && !spawn_failed && !stop_requested && !startup_stalled {
                let state = entry
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                (state.logs.evidence().boot_observed || status.is_none())
                    && entry.telemetry.observe_startup(
                        state.logs.evidence().boot_observed,
                        state.logs.has_entries(),
                        state
                            .process_started
                            .map_or(Duration::ZERO, |started| started.elapsed()),
                    )
            } else {
                false
            };
        if observed_now {
            startup_observed = true;
            if let Some(instance) = &instance {
                let instance = instance.clone();
                let launched_at = launched_at.clone();
                if !matches!(
                    tokio::task::spawn_blocking(move || {
                        instance.record_successful_launch(&launched_at)
                    })
                    .await,
                    Ok(Ok(()))
                ) {
                    // Recency is best effort, as in the retained launcher. Its
                    // atomic failure leaves no file effect to keep unsettled.
                    tracing::warn!("Successful launch recency could not be saved.");
                }
            }
        }
        let unresolved = output_failed
            || settlement_failed
            || settling_since.is_some_and(|started| started.elapsed() >= SETTLEMENT_DEADLINE);
        entry.publish(|snapshot| {
            snapshot.exit_code = status.and_then(|status| status.code());
            snapshot.tree_settled = tree_settled;
            snapshot.process_alive = !tree_settled;
            snapshot.output_drained = drained;
            snapshot.phase = if unresolved {
                SessionPhase::Unresolved
            } else if stop_requested {
                SessionPhase::Stopping
            } else if settling_since.is_some() {
                SessionPhase::Settling
            } else if startup_observed {
                SessionPhase::Running
            } else {
                SessionPhase::Starting
            };
        });
        if status.is_some() && tree_settled && drained {
            entry.telemetry.failure(
                (!spawn_failed && !stop_requested)
                    .then_some(TelemetryErrorKind::LaunchStartupFailed),
            );
            if !super::prepare::try_settle_natives(natives.clone()).await {
                settlement_failed = true;
                entry.publish(|snapshot| {
                    snapshot.phase = SessionPhase::Unresolved;
                    snapshot.outcome = None;
                });
                ticker.tick().await;
                continue;
            }
            let outcome = entry.persist_settlement(
                SettledProcess::ChildExited {
                    exit_code: status.and_then(|status| status.code()),
                    signal: status.and_then(exit_signal),
                    stop_requested,
                    was_running: startup_observed,
                    spawn_failed,
                },
                startup_stalled,
            );
            if outcome.is_none() {
                settlement_failed = true;
                entry.publish(|snapshot| {
                    snapshot.phase = SessionPhase::Unresolved;
                    snapshot.outcome = None;
                });
                ticker.tick().await;
                continue;
            }
            entry
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .logs
                .clear_secrets();
            entry.publish(|snapshot| {
                snapshot.phase = SessionPhase::Exited;
                snapshot.stop_allowed = false;
                snapshot.outcome = outcome;
            });
            return;
        }
        let ProcessOutput { stdout, stderr } = &mut output;
        tokio::select! {
            _ = ticker.tick() => {},
            _ = stop.changed(), if !stop_requested && !startup_stalled => {},
            _ = cancellation.cancelled(), if !stop_requested && !startup_stalled => {},
            read = stdout.read(&mut stdout_buffer), if !stdout_finished => {
                match read {
                    Ok(0) => { stdout_finished = true; entry.output(LogStream::Stdout, None); },
                    Ok(count) => entry.output(LogStream::Stdout, Some(&stdout_buffer[..count])),
                    Err(_) => { stdout_finished = true; output_failed = true; },
                }
            },
            read = stderr.read(&mut stderr_buffer), if !stderr_finished => {
                match read {
                    Ok(0) => { stderr_finished = true; entry.output(LogStream::Stderr, None); },
                    Ok(count) => entry.output(LogStream::Stderr, Some(&stderr_buffer[..count])),
                    Err(_) => { stderr_finished = true; output_failed = true; },
                }
            },
        }
    }
}

async fn finish_without_process(
    entry: &SessionEntry,
    stopped: bool,
    natives: Option<Arc<crate::install::vanilla::PreparedNatives>>,
) {
    entry.telemetry.failure(None);
    entry.output(LogStream::Stdout, None);
    entry.output(LogStream::Stderr, None);
    while !super::prepare::try_settle_natives(natives.clone()).await {
        entry.publish(|snapshot| {
            snapshot.phase = SessionPhase::Unresolved;
            snapshot.tree_settled = true;
            snapshot.process_alive = false;
            snapshot.output_drained = true;
            snapshot.outcome = None;
        });
        tokio::time::sleep(PROCESS_POLL).await;
    }
    let outcome = loop {
        if let Some(outcome) = entry.persist_settlement(SettledProcess::NoChild { stopped }, false)
        {
            break Some(outcome);
        }
        entry.publish(|snapshot| {
            snapshot.phase = SessionPhase::Unresolved;
            snapshot.outcome = None;
        });
        tokio::time::sleep(PROCESS_POLL).await;
    };
    entry
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .logs
        .clear_secrets();
    entry.publish(|snapshot| {
        snapshot.phase = SessionPhase::Exited;
        snapshot.stop_allowed = false;
        snapshot.process_alive = false;
        snapshot.tree_settled = true;
        snapshot.output_drained = true;
        snapshot.outcome = outcome;
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
fn entry_for_instance(
    reports: LaunchReportStore,
    telemetry: Option<Arc<Telemetry>>,
    instance_id: InstanceId,
) -> Arc<SessionEntry> {
    let snapshot = SessionSnapshot::starting(instance_id, uuid::Uuid::new_v4().to_string());
    Arc::new(SessionEntry {
        state: Mutex::new(EntryState {
            snapshot: snapshot.clone(),
            logs: LogCollector::new(vec!["session-secret".into()]),
            report: None,
            process_started: None,
            boot_duration_ms: None,
            observation: None,
            report_unavailable: false,
        }),
        snapshot: watch::channel(snapshot.clone()).0,
        log_events: broadcast::channel(16).0,
        stop: watch::channel(false).0,
        changes: watch::channel(0).0,
        command_inspection: SessionCommandInspection {
            session_id: snapshot.session_id,
            command_arg_count: 0,
            java_path_present: true,
        },
        reports: Some(reports),
        version_id: "1.21.1".into(),
        scenario: LaunchProofScenario {
            performance_mode: "vanilla".into(),
            ..Default::default()
        },
        telemetry: LaunchAttemptTelemetry::started(telemetry, "vanilla"),
        acceptance: None,
    })
}

#[cfg(test)]
pub(super) async fn finish_unstarted_for_test(
    acceptance: super::coordinator::AcceptedIntent,
    session_id: String,
    instance_id: InstanceId,
    version_id: String,
    reports: LaunchReportStore,
    save_report: bool,
) -> (SessionSnapshot, Option<LaunchProofRecord>) {
    let mut entry = entry_for_instance(reports, None, instance_id);
    let entry = Arc::get_mut(&mut entry).unwrap();
    entry.state.get_mut().unwrap().snapshot.session_id = session_id.clone();
    entry
        .snapshot
        .send_replace(entry.state.get_mut().unwrap().snapshot.clone());
    entry.command_inspection.session_id = session_id;
    entry.version_id = version_id;
    entry.scenario.version_id = Some(entry.version_id.clone());
    entry.acceptance = Some(acceptance);
    if !save_report {
        entry.reports = None;
    }
    finish_without_process(entry, false, None).await;
    (
        entry.current(),
        entry.state.get_mut().unwrap().report.clone(),
    )
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn native_fixture() -> (
        tempfile::TempDir,
        axial_minecraft::managed_path::ManagedLibraryTestAuthority,
        Arc<crate::install::vanilla::PreparedNatives>,
    ) {
        use sha1::{Digest, Sha1};
        use std::io::Write;
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let authority =
            axial_minecraft::managed_path::ManagedLibraryTestAuthority::open(root.path()).unwrap();
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file(
                "fixture-native.bin",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"owned native fixture").unwrap();
        let archive = archive.finish().unwrap().into_inner();
        std::fs::create_dir_all(root.path().join("libraries/fixture")).unwrap();
        std::fs::write(root.path().join("libraries/fixture/native.jar"), &archive).unwrap();
        let version = serde_json::from_value(serde_json::json!({
            "id": "fixture", "libraries": [{
                "name": "fixture:native:1",
                "natives": {"osx":"fixture-native", "linux":"fixture-native", "windows":"fixture-native"},
                "downloads": {"classifiers":{"fixture-native":{
                    "path":"fixture/native.jar", "sha1":format!("{:x}", Sha1::digest(&archive)),
                    "size":archive.len(), "url":"https://invalid.example/native.jar"
                }}}
            }]
        })).unwrap();
        let natives = crate::install::vanilla::prepare_natives(
            authority.operation(),
            root.path(),
            &version,
            &axial_minecraft::default_environment(),
        )
        .await
        .unwrap()
        .unwrap();
        (root, authority, Arc::new(natives))
    }

    fn entry(reports: LaunchReportStore) -> Arc<SessionEntry> {
        entry_with_telemetry(reports, None)
    }

    fn entry_with_telemetry(
        reports: LaunchReportStore,
        telemetry: Option<Arc<Telemetry>>,
    ) -> Arc<SessionEntry> {
        entry_for_instance(reports, telemetry, InstanceId::new())
    }

    fn accepted_entry(storage: Arc<crate::storage::MetadataStore>) -> Arc<SessionEntry> {
        let (acceptance, session_id, instance_id) =
            super::super::coordinator::accepted_for_test(storage.clone());
        let reports = LaunchReportStore::new(storage.clone()).unwrap();
        let mut entry = entry_for_instance(reports.clone(), None, instance_id);
        let owned = Arc::get_mut(&mut entry).unwrap();
        owned.state.get_mut().unwrap().snapshot.session_id = session_id.clone();
        owned
            .snapshot
            .send_replace(owned.state.get_mut().unwrap().snapshot.clone());
        owned.command_inspection.session_id = session_id;
        owned.scenario.version_id = Some(owned.version_id.clone());
        owned.acceptance = Some(acceptance);
        entry
    }

    async fn start_watchdog_fixture(
        script: &str,
        marker: Option<&std::path::Path>,
    ) -> (
        Arc<SessionEntry>,
        LaunchReportStore,
        tokio::task::JoinHandle<()>,
    ) {
        let storage = Arc::new(crate::storage::MetadataStore::in_memory().unwrap());
        let entry = accepted_entry(storage);
        let reports = entry.reports.as_ref().unwrap().clone();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script, "watchdog-fixture"]);
        if let Some(marker) = marker {
            command.arg(marker);
        }
        let (process, output) = match OwnedProcess::spawn(command) {
            Ok(value) => value,
            Err(_) => panic!("watchdog fixture spawn"),
        };
        let worker_entry = entry.clone();
        let worker = tokio::spawn(async move {
            supervise_process(
                process,
                output,
                false,
                worker_entry.clone(),
                CancellationToken::new(),
                worker_entry.stop.subscribe(),
                None,
                None,
            )
            .await;
        });
        let mut snapshots = entry.snapshot.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            while snapshots.borrow_and_update().started_at_ms.is_none() {
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        (entry, reports, worker)
    }

    async fn finish_watchdog_fixture(
        entry: &SessionEntry,
        reports: &LaunchReportStore,
        worker: tokio::task::JoinHandle<()>,
        reason: super::super::outcome::SessionExitReason,
    ) {
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("watchdog settlement deadline")
            .unwrap();
        let snapshot = entry.current();
        assert_eq!(snapshot.phase, SessionPhase::Exited);
        assert!(snapshot.tree_settled && snapshot.output_drained);
        assert!(!snapshot.process_alive && !snapshot.stop_allowed);
        let outcome = snapshot.outcome.unwrap();
        assert_eq!(outcome.reason, reason);
        assert_eq!(
            reports
                .get(&snapshot.session_id)
                .unwrap()
                .unwrap()
                .session_outcome,
            outcome,
        );
    }

    #[tokio::test]
    async fn startup_watchdog_settles_a_silent_process_tree_after_thirty_seconds() {
        use super::super::outcome::{FailureClass, SessionExitReason};
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("descendant.pid");
        let (entry, reports, worker) = start_watchdog_fixture(
            "sleep 120 & printf '%s' \"$!\" > \"$1\"; wait",
            Some(&marker),
        )
        .await;
        let descendant = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = pid.parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // SAFETY: this positive PID came from the fixture's child; zero only probes it.
        assert_eq!(unsafe { libc::kill(descendant, 0) }, 0);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert_eq!(entry.current().phase, SessionPhase::Starting);
        assert!(!entry.current().view_model.playing);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::time::resume();
        finish_watchdog_fixture(&entry, &reports, worker, SessionExitReason::StartupStalled).await;
        assert_eq!(
            entry.current().outcome.unwrap().failure_class,
            Some(FailureClass::StartupStalled),
        );
        assert!(entry.state.lock().unwrap().logs.entries().is_empty());
        assert_eq!(unsafe { libc::kill(descendant, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn startup_watchdog_accepts_ordinary_output_and_keeps_playing_after_sixty_seconds() {
        use super::super::outcome::SessionExitReason;
        let (entry, reports, worker) = start_watchdog_fixture(
            "sleep 120 & printf 'ordinary startup output\\n'; wait",
            None,
        )
        .await;
        let mut logs = entry.log_events.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !entry.state.lock().unwrap().logs.has_entries() {
                logs.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        let group = i32::try_from(entry.current().pid.unwrap()).unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert_eq!(entry.current().view_model.state_id, "starting");
        assert!(!entry.current().view_model.playing);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let snapshot = entry.current();
        assert_eq!(snapshot.phase, SessionPhase::Running);
        assert_eq!(snapshot.view_model.state_id, "running");
        assert_eq!(snapshot.view_model.label, "Playing");
        assert!(snapshot.view_model.playing);
        assert!(!snapshot.boot_observed);
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(entry.current().phase, SessionPhase::Running);
        assert!(entry.current().view_model.playing && entry.current().process_alive);
        assert!(!worker.is_finished());
        entry.stop();
        tokio::time::resume();
        finish_watchdog_fixture(&entry, &reports, worker, SessionExitReason::LauncherStopped).await;
        assert!(!entry.current().boot_observed);
        assert!(
            reports
                .get(&entry.current().session_id)
                .unwrap()
                .unwrap()
                .logs
                .iter()
                .any(|line| line.text == "ordinary startup output"),
        );
        // SAFETY: the group belongs to the fixture and signal zero only probes it.
        assert_eq!(unsafe { libc::kill(-group, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn startup_watchdog_accepts_boot_observed_before_the_deadline() {
        use super::super::outcome::SessionExitReason;
        let (entry, reports, worker) = start_watchdog_fixture("exec sleep 120", None).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(29)).await;
        entry.output(
            LogStream::Stdout,
            Some(b"[Render thread/INFO]: LWJGL Version: 3.3.3\n"),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let snapshot = entry.current();
        assert_eq!(snapshot.phase, SessionPhase::Running);
        assert!(snapshot.boot_observed && snapshot.process_alive);
        entry.stop();
        tokio::time::resume();
        finish_watchdog_fixture(&entry, &reports, worker, SessionExitReason::LauncherStopped).await;
    }

    #[tokio::test]
    async fn startup_watchdog_and_stop_preserve_the_first_accepted_cause() {
        use super::super::outcome::SessionExitReason;
        for stop_first in [true, false] {
            let (entry, reports, worker) = start_watchdog_fixture("exec sleep 120", None).await;
            tokio::time::pause();
            if stop_first {
                tokio::time::advance(Duration::from_secs(29)).await;
                entry.stop();
                tokio::time::advance(Duration::from_secs(2)).await;
            } else {
                tokio::time::advance(Duration::from_secs(31)).await;
                // Keep virtual time fixed while the owner accepts the deadline.
                for _ in 0..100 {
                    if entry.current().phase != SessionPhase::Starting {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert_ne!(entry.current().phase, SessionPhase::Starting);
                entry.stop();
                entry.output(
                    LogStream::Stdout,
                    Some(b"LWJGL Version: late boot output\n"),
                );
            }
            tokio::time::resume();
            finish_watchdog_fixture(
                &entry,
                &reports,
                worker,
                if stop_first {
                    SessionExitReason::LauncherStopped
                } else {
                    SessionExitReason::StartupStalled
                },
            )
            .await;
        }
    }

    fn recency_fixture() -> (
        tempfile::TempDir,
        RegisteredInstance,
        Arc<crate::storage::MetadataStore>,
    ) {
        use crate::{
            files::PortableName,
            instances::{
                directory::{InstanceDirectories, MIGRATION, Registry},
                model::{Instance, InstanceResult},
            },
            library::{LibraryLifecycle, LibraryOpenOutcome},
            settings::InstanceSettings,
            storage::MetadataStore,
            tasks::Exclusions,
        };
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let id = InstanceId::new();
        std::fs::create_dir_all(root.path().join("instances").join(id.as_str())).unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated fixture library: {other:?}"),
        };
        let pin = library.admit().unwrap();
        let directory = pin
            .files()
            .unwrap()
            .open_directory(&PortableName::new_exact("instances").unwrap())
            .unwrap()
            .open_directory(&PortableName::new_exact(id.as_str()).unwrap())
            .unwrap();
        let storage = Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
        storage.migrate(&[MIGRATION]).unwrap();
        let registry = Registry::new(storage.clone());
        let record = storage
            .transaction(|tx| -> InstanceResult<_> {
                let reserved = registry.reserve(
                    tx,
                    Instance {
                        id: id.clone(),
                        name: "Launch recency".into(),
                        version_id: "1.21.1".into(),
                        created_at: "2026-09-26T09:00:00.000Z".into(),
                        last_played_at: "2026-09-26T10:00:00.000Z".into(),
                        art_seed: 42,
                        settings: InstanceSettings::default(),
                        icon: String::new(),
                        accent: String::new(),
                        loader_key: "vanilla".into(),
                        minecraft_version: "1.21.1".into(),
                        revision: 0,
                    },
                    &pin.library_id().to_string(),
                )?;
                registry.commit_reserved(tx, &reserved, &directory.receipt().unwrap())
            })
            .unwrap();
        let exclusion = Exclusions::new().try_acquire([id.as_str()], []).unwrap();
        let admitted = InstanceDirectories::admit_record(registry, record, pin, exclusion).unwrap();
        (root, admitted, storage)
    }

    fn start_recency_fixture(
        instance: RegisteredInstance,
        storage: Arc<crate::storage::MetadataStore>,
        script: &str,
    ) -> (
        Arc<SessionEntry>,
        SessionManager,
        tokio::task::JoinHandle<()>,
    ) {
        let manager = SessionManager::new(TaskOwner::new(2).unwrap());
        let reports = LaunchReportStore::new(storage).unwrap();
        let mut entry = entry_for_instance(reports, None, instance.record().instance.id.clone());
        Arc::get_mut(&mut entry).unwrap().changes = manager.changes.clone();
        let snapshot = entry.current();
        {
            let mut registry = manager.registry.lock().unwrap();
            registry
                .current
                .insert(snapshot.instance_id, snapshot.session_id.clone());
            registry.sessions.insert(snapshot.session_id, entry.clone());
        }
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        let (process, output) = match OwnedProcess::spawn(command) {
            Ok(value) => value,
            Err(_) => panic!("recency fixture process"),
        };
        let worker_entry = entry.clone();
        let worker = tokio::spawn(async move {
            supervise_process(
                process,
                output,
                false,
                worker_entry.clone(),
                CancellationToken::new(),
                worker_entry.stop.subscribe(),
                None,
                Some(instance),
            )
            .await;
        });
        (entry, manager, worker)
    }

    #[tokio::test]
    async fn successful_startup_recency_survives_shutdown_and_reopen_without_duplicate_writes() {
        use crate::instances::directory::Registry;
        let (root, admitted, storage) = recency_fixture();
        let before = admitted.record().clone();
        let registry = Registry::new(storage.clone());
        assert!(registry.last_instance_id().unwrap().is_none());
        let (entry, manager, worker) = start_recency_fixture(
            admitted,
            storage.clone(),
            "printf '[Render thread/INFO]: LWJGL Version: 3.3.3\\n'; sleep 30",
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while registry.get_live(&before.instance.id).unwrap().revision == before.revision {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let launched_at = entry.current().launched_at;
        manager.shutdown(Duration::from_secs(5)).await.unwrap();
        worker.await.unwrap();
        drop((entry, manager, registry, storage));

        let storage = Arc::new(
            crate::storage::MetadataStore::open(root.path().join("metadata.sqlite")).unwrap(),
        );
        let registry = Registry::new(storage);
        let stored = registry.get_live(&before.instance.id).unwrap();
        assert_eq!(stored.instance.last_played_at, launched_at);
        assert_eq!(
            registry.last_instance_id().unwrap(),
            Some(before.instance.id.clone())
        );
        assert_eq!(stored.revision, before.revision + 1);
        assert_eq!(stored.instance.settings, before.instance.settings);
    }

    #[tokio::test]
    async fn failed_startup_preserves_prior_recency_after_reopen() {
        use crate::instances::directory::Registry;
        let (root, admitted, storage) = recency_fixture();
        let before = admitted.record().clone();
        let (entry, manager, worker) = start_recency_fixture(
            admitted,
            storage.clone(),
            "printf 'early failure\\n' >&2; exit 1",
        );
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        manager.shutdown(Duration::from_secs(5)).await.unwrap();
        assert!(!entry.current().boot_observed);
        drop((entry, manager, storage));

        let registry = Registry::new(Arc::new(
            crate::storage::MetadataStore::open(root.path().join("metadata.sqlite")).unwrap(),
        ));
        assert_eq!(registry.get_live(&before.instance.id).unwrap(), before);
        assert!(registry.last_instance_id().unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_recency_write_is_atomic_and_does_not_block_shutdown_or_retry_later() {
        use crate::{instances::directory::Registry, storage::StorageError};
        let (_root, admitted, storage) = recency_fixture();
        let before = admitted.record().clone();
        storage
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch(
                    "CREATE TRIGGER reject_launch_recency BEFORE UPDATE ON instance_selection
                     BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        let (entry, manager, worker) = start_recency_fixture(
            admitted,
            storage.clone(),
            "printf '[Render thread/INFO]: LWJGL Version: 3.3.3\\n'; sleep 0.2; printf 'after-startup\\n'; sleep 30",
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observed = entry
                    .state
                    .lock()
                    .unwrap()
                    .logs
                    .entries()
                    .iter()
                    .any(|line| line.text.contains("after-startup"));
                if observed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        storage
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch("DROP TRIGGER reject_launch_recency;")?;
                Ok(())
            })
            .unwrap();
        manager.shutdown(Duration::from_secs(5)).await.unwrap();
        worker.await.unwrap();
        let registry = Registry::new(storage);
        assert_eq!(registry.get_live(&before.instance.id).unwrap(), before);
        assert!(registry.last_instance_id().unwrap().is_none());
        assert_eq!(entry.current().phase, SessionPhase::Exited);
    }

    #[tokio::test]
    async fn attempt_telemetry_is_consent_gated_and_startup_completion_is_once_only() {
        let telemetry = Telemetry::configured_for_test();
        let disabled = LaunchAttemptTelemetry::started(Some(telemetry.clone()), "fabric");
        disabled.failure(Some(TelemetryErrorKind::LaunchSpawnFailed));
        assert!(telemetry.queued_events_for_test().is_empty());
        telemetry
            .consent_change()
            .await
            .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
        let attempt = LaunchAttemptTelemetry::started(Some(telemetry.clone()), "fabric");
        attempt.observe_startup(false, false, Duration::from_secs(60));
        attempt.observe_startup(false, true, Duration::from_secs(29));
        assert_eq!(
            telemetry.queued_events_for_test(),
            vec![TelemetryEvent::LaunchStarted {
                loader: Some(TelemetryLoader::Fabric)
            }]
        );
        attempt.observe_startup(false, true, Duration::from_secs(30));
        attempt.observe_startup(true, true, Duration::from_secs(31));
        attempt.failure(Some(TelemetryErrorKind::LaunchStartupFailed));
        assert_eq!(
            telemetry.queued_events_for_test(),
            vec![
                TelemetryEvent::LaunchStarted {
                    loader: Some(TelemetryLoader::Fabric)
                },
                TelemetryEvent::LaunchCompleted {
                    outcome: TelemetryLaunchOutcome::Success
                },
            ]
        );
    }

    #[tokio::test]
    async fn preboot_exit_emits_one_startup_failure_but_user_stop_has_no_error_event() {
        let telemetry = Telemetry::configured_for_test();
        telemetry
            .consent_change()
            .await
            .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
        let reports = LaunchReportStore::new(Arc::new(
            crate::storage::MetadataStore::in_memory().unwrap(),
        ))
        .unwrap();
        let failed = entry_with_telemetry(reports.clone(), Some(telemetry.clone()));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'early failure\\n' >&2; exit 1"]);
        let (process, output) = match OwnedProcess::spawn(command) {
            Ok(value) => value,
            Err(_) => panic!("fixture spawn"),
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            supervise_process(
                process,
                output,
                false,
                failed.clone(),
                CancellationToken::new(),
                failed.stop.subscribe(),
                None,
                None,
            ),
        )
        .await
        .unwrap();
        let stopped = entry_with_telemetry(reports, Some(telemetry.clone()));
        finish_without_process(&stopped, true, None).await;
        assert_eq!(
            telemetry.queued_events_for_test(),
            vec![
                TelemetryEvent::LaunchStarted {
                    loader: Some(TelemetryLoader::Vanilla)
                },
                TelemetryEvent::LaunchCompleted {
                    outcome: TelemetryLaunchOutcome::Failure
                },
                TelemetryEvent::ErrorCaptured {
                    kind: TelemetryErrorKind::LaunchStartupFailed
                },
                TelemetryEvent::LaunchStarted {
                    loader: Some(TelemetryLoader::Vanilla)
                },
                TelemetryEvent::LaunchCompleted {
                    outcome: TelemetryLaunchOutcome::Failure
                },
            ]
        );
    }

    #[tokio::test]
    async fn real_output_is_drained_and_persisted_before_terminal_state() {
        let telemetry = Telemetry::configured_for_test();
        telemetry
            .consent_change()
            .await
            .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
        let (_root, _authority, natives) = native_fixture().await;
        let native_path = natives.path().to_owned();
        let storage = Arc::new(crate::storage::MetadataStore::in_memory().unwrap());
        let reports = LaunchReportStore::new(storage).unwrap();
        let entry = entry_with_telemetry(reports.clone(), Some(telemetry.clone()));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '[Render thread/INFO]: LWJGL Version: 3.3.3\\n'; printf 'session-secret\\n' >&2"]);
        let (process, output) = match OwnedProcess::spawn(command) {
            Ok(value) => value,
            Err(_) => panic!("spawn fixture"),
        };
        let stop = entry.stop.subscribe();
        tokio::time::timeout(
            Duration::from_secs(5),
            supervise_process(
                process,
                output,
                false,
                entry.clone(),
                CancellationToken::new(),
                stop,
                Some(natives),
                None,
            ),
        )
        .await
        .unwrap();
        let snapshot = entry.current();
        assert_eq!(snapshot.phase, SessionPhase::Exited);
        assert!(snapshot.tree_settled && snapshot.output_drained && snapshot.boot_observed);
        assert!(
            !native_path.exists(),
            "terminal state must follow exact native cleanup"
        );
        assert_eq!(
            telemetry.queued_events_for_test(),
            vec![
                TelemetryEvent::LaunchStarted {
                    loader: Some(TelemetryLoader::Vanilla)
                },
                TelemetryEvent::LaunchCompleted {
                    outcome: TelemetryLaunchOutcome::Success
                },
            ]
        );
        let report = reports.get(&snapshot.session_id).unwrap().unwrap();
        assert!(report.boot_duration_ms.is_some());
        assert!(report.logs.iter().any(|entry| entry.text.contains("LWJGL")));
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("session-secret")
        );
    }

    #[tokio::test]
    async fn unexpected_native_file_blocks_terminal_state_and_is_preserved() {
        let (_root, _authority, natives) = native_fixture().await;
        let native_path = natives.path().to_owned();
        let unexpected_file = native_path.join("user-file");
        std::fs::write(&unexpected_file, b"user payload").unwrap();
        let storage = Arc::new(crate::storage::MetadataStore::in_memory().unwrap());
        let entry = accepted_entry(storage.clone());
        let reports = entry.reports.as_ref().unwrap().clone();
        let worker = {
            let entry = entry.clone();
            tokio::spawn(async move { finish_without_process(&entry, true, Some(natives)).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while entry.current().phase != SessionPhase::Unresolved {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&unexpected_file).unwrap(), b"user payload");
        assert!(!worker.is_finished());
        assert!(entry.state.lock().unwrap().observation.is_none());
        let settled = || {
            storage
                .read(|db| -> Result<i64, crate::storage::StorageError> {
                    Ok(db.query_row(
                        "SELECT count(*) FROM launch_intents WHERE settlement IS NOT NULL",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .unwrap()
        };
        assert_eq!(settled(), 0);
        assert!(reports.get(&entry.current().session_id).unwrap().is_none());
        // Model the user moving their unrelated file out of the owned cache.
        std::fs::remove_file(&unexpected_file).unwrap();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.current().phase, SessionPhase::Exited);
        assert!(!native_path.exists());
        assert!(entry.state.lock().unwrap().observation.is_some());
        assert_eq!(settled(), 1);
    }

    #[tokio::test]
    async fn failed_report_write_retains_nonterminal_session_until_retry_succeeds() {
        let storage = Arc::new(crate::storage::MetadataStore::in_memory().unwrap());
        let reports = LaunchReportStore::new(storage.clone()).unwrap();
        let entry = entry(reports);
        storage
            .transaction(|tx| -> Result<(), crate::storage::StorageError> {
                tx.execute_batch(
                    "ALTER TABLE launch_reports RENAME TO unavailable_launch_reports;",
                )?;
                Ok(())
            })
            .unwrap();
        let worker = {
            let entry = entry.clone();
            tokio::spawn(async move { finish_without_process(&entry, true, None).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while entry.current().phase != SessionPhase::Unresolved {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(entry.current().outcome.is_none());
        assert!(!worker.is_finished());
        storage
            .transaction(|tx| -> Result<(), crate::storage::StorageError> {
                tx.execute_batch(
                    "ALTER TABLE unavailable_launch_reports RENAME TO launch_reports;",
                )?;
                Ok(())
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.current().phase, SessionPhase::Exited);
    }
}
