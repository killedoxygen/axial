mod classify;
mod priority;
mod supervisor;

use crate::execution::process::{
    ProcessKillReason, ProcessKillRequest, ProcessObservation, ProcessObservationRequest,
    ProcessStopIntent, ProcessStopRequest, observe_process, process_killed, process_session_target,
    process_stage_evidence, process_stop_requested,
};
use axial_launcher::{
    LaunchEvent, LaunchFailure, LaunchFailureClass, LaunchLogEvent, LaunchNotice,
    LaunchPriorityEvidence, LaunchSessionOutcomeKind, LaunchSessionRecord, LaunchStageEvidence,
    LaunchStageRecord, LaunchState, LaunchStatusEvent, classify_startup_failure_text,
    launch_notice_from_values, launch_stage_label, launch_state_name,
};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, OwnedMutexGuard, RwLock, broadcast};

const MAX_GUARDIAN_STAGE_DETAILS: usize = 8;
const MAX_STAGE_EVIDENCE: usize = 16;
const MAX_STAGE_EVIDENCE_DETAILS: usize = 8;
const MAX_STAGE_NOTE_CHARS: usize = 160;
const MAX_NOTICE_MESSAGE_CHARS: usize = 180;
const MAX_NOTICE_DETAIL_CHARS: usize = 240;
const MAX_NOTICE_DETAILS: usize = 8;
const MAX_LAUNCH_LOG_LINE_CHARS: usize = 1_000;
const MAX_RETAINED_TERMINAL_SESSIONS: usize = 32;
const PRIVATE_NOTICE_FALLBACK: &str = "Launch status details were hidden for privacy.";
const FAILURE_SIGNAL_VALID_FOR_EXIT_MS: u64 = 15_000;

pub(super) struct ProcessAttemptScope {
    pub(super) id: u64,
    log_transition: Mutex<()>,
}

impl ProcessAttemptScope {
    fn new(id: u64) -> Arc<Self> {
        Arc::new(Self {
            id,
            log_transition: Mutex::new(()),
        })
    }
}

struct SessionEntry {
    attempt: Arc<ProcessAttemptScope>,
    process: Option<supervisor::ProcessControlHandle>,
    record: LaunchSessionRecord,
    events: broadcast::Sender<LaunchEvent>,
    observed_failure: Option<ObservedFailureSignal>,
    log_count: usize,
    stop_requested: bool,
    retention_holds: usize,
    terminal_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObservedFailureSignal {
    class: LaunchFailureClass,
    observed_at_ms: u64,
}

impl ObservedFailureSignal {
    fn fresh_for_exit(self, exit_observed_at_ms: u64) -> Option<LaunchFailureClass> {
        let age_ms = exit_observed_at_ms.saturating_sub(self.observed_at_ms);
        (age_ms <= FAILURE_SIGNAL_VALID_FOR_EXIT_MS).then_some(self.class)
    }
}

struct PreparedLogLine {
    observed_at_ms: u64,
    boot_evidence: Option<Vec<LaunchStageEvidence>>,
    failure_class: Option<LaunchFailureClass>,
    event: LaunchLogEvent,
}

struct RawLogLine {
    source: String,
    text: String,
    observed_at_ms: u64,
}

pub(super) struct ProcessExitContext {
    pub(super) record: LaunchSessionRecord,
    pub(super) observed_failure: Option<LaunchFailureClass>,
}

#[derive(Clone)]
struct BootPriorityPromotionTicket {
    attempt: Arc<ProcessAttemptScope>,
    process: Option<supervisor::ProcessControlHandle>,
    pid: Option<u32>,
}

#[derive(Clone, Copy)]
enum BootPromotionGate {
    Stale,
    Resolve,
    CompleteSkipped(&'static str),
    Promote,
}

fn attempt_scopes_match(
    current: &Arc<ProcessAttemptScope>,
    expected: &Arc<ProcessAttemptScope>,
) -> bool {
    current.id == expected.id
}

fn process_state_regresses(previous: LaunchState, next: LaunchState) -> bool {
    (classify::is_terminal_state(previous)
        && matches!(
            next,
            LaunchState::Starting
                | LaunchState::Monitoring
                | LaunchState::Running
                | LaunchState::Degraded
        ))
        || (matches!(previous, LaunchState::Running | LaunchState::Degraded)
            && matches!(next, LaunchState::Starting | LaunchState::Monitoring))
        || (previous == LaunchState::Monitoring && next == LaunchState::Starting)
}

fn boot_promotion_gate(
    entry: Option<&SessionEntry>,
    ticket: &BootPriorityPromotionTicket,
) -> BootPromotionGate {
    let Some(entry) = entry else {
        return BootPromotionGate::Stale;
    };
    if entry.record.pid != ticket.pid || !attempt_scopes_match(&entry.attempt, &ticket.attempt) {
        return BootPromotionGate::Stale;
    }
    if entry.record.boot_completed_at_ms.is_some() {
        return BootPromotionGate::Resolve;
    }
    if !matches!(
        entry.record.state,
        LaunchState::Starting | LaunchState::Monitoring
    ) {
        return BootPromotionGate::Resolve;
    }
    if entry.stop_requested {
        return BootPromotionGate::CompleteSkipped("skipped_stop_requested");
    }
    if ticket.pid.is_some() && ticket.process.is_none() {
        return BootPromotionGate::CompleteSkipped("skipped_missing_process_handle");
    }
    BootPromotionGate::Promote
}

pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionEntry>>,
    active_processes: Mutex<HashMap<u64, supervisor::ProcessControlHandle>>,
    process_owner_changes: Notify,
    lifecycle_transition: Arc<Mutex<()>>,
    shutdown_started: AtomicBool,
    changes: broadcast::Sender<()>,
    next_attempt_id: AtomicU64,
    next_terminal_sequence: AtomicU64,
}

#[derive(Debug, thiserror::Error)]
#[error("launch session store is shutting down")]
pub struct SessionAdmissionError;

#[derive(Debug, thiserror::Error)]
pub enum SessionStopError {
    #[error("session not found")]
    SessionNotFound,
    #[error("session has no running process")]
    NoLiveProcess,
    #[error("launch process stop failed")]
    Process(#[source] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupOutcome {
    Stable,
    Exited,
    TimedOut,
    Stalled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchFailureTerminationErrorClass {
    MissingProcess,
    OwnerUnavailable,
    SettlementUnavailable,
    StaleAttempt,
    TerminationRejected,
}

impl LaunchFailureTerminationErrorClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MissingProcess => "missing_process",
            Self::OwnerUnavailable => "owner_unavailable",
            Self::SettlementUnavailable => "settlement_unavailable",
            Self::StaleAttempt => "stale_attempt",
            Self::TerminationRejected => "termination_rejected",
        }
    }
}

pub(crate) enum LaunchFailureTermination {
    Ready(LaunchFailureTerminalizationLease),
    Pending(PendingLaunchFailureTermination),
    Unconfirmed(LaunchFailureTerminationErrorClass),
}

pub(crate) struct PendingLaunchFailureTermination {
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
    request: supervisor::ProcessTerminationRequest,
}

pub(crate) struct LaunchFailureTerminalizationLease {
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
    lifecycle_guard: Option<OwnedMutexGuard<()>>,
}

impl PendingLaunchFailureTermination {
    pub(crate) fn error_class(&self) -> LaunchFailureTerminationErrorClass {
        LaunchFailureTerminationErrorClass::TerminationRejected
    }

    pub(crate) async fn wait_for_settlement(
        mut self,
    ) -> Result<LaunchFailureTerminalizationLease, LaunchFailureTerminationErrorClass> {
        let settled = self.request.settled().await;
        if settled.is_err() && !self.request.terminal_is_settled() {
            return Err(LaunchFailureTerminationErrorClass::SettlementUnavailable);
        }

        self.store
            .acquire_launch_failure_terminalization_lease(&self.session_id, self.attempt, None)
            .await
    }
}

impl LaunchFailureTerminalizationLease {
    pub(crate) async fn release(mut self) {
        self.store
            .release_terminal_retention_hold_for_attempt(&self.session_id, &self.attempt)
            .await;
        drop(self.lifecycle_guard.take());
    }
}

#[must_use]
pub(crate) struct UserStopLease {
    store: Arc<SessionStore>,
    lifecycle_guard: Option<OwnedMutexGuard<()>>,
    attempt: Arc<ProcessAttemptScope>,
    session_id: String,
    record: LaunchSessionRecord,
    retention_hold_active: bool,
}

struct PendingUserStop {
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
    request: Option<supervisor::ProcessTerminationRequest>,
    lifecycle_guard: Option<OwnedMutexGuard<()>>,
    prior_terminal_sequence: Option<Option<u64>>,
    acceptance_rejected: bool,
}

impl PendingUserStop {
    async fn accepted(&mut self) -> std::io::Result<supervisor::ProcessTerminationAcceptance> {
        let acceptance = self
            .request
            .as_mut()
            .expect("pending user stop request was already settled")
            .accepted()
            .await;
        self.acceptance_rejected = acceptance.is_err();
        acceptance
    }

    async fn publish_intent(
        &self,
        acceptance: supervisor::ProcessTerminationAcceptance,
    ) -> Option<LaunchSessionRecord> {
        if !user_stop_was_accepted(acceptance) {
            return None;
        }
        self.store
            .record_user_stop_intent_for_attempt(&self.session_id, &self.attempt)
            .await
    }

    async fn reaped(&mut self) -> std::io::Result<supervisor::ProcessTerminationAcceptance> {
        self.request
            .as_mut()
            .expect("pending user stop request was already settled")
            .reaped()
            .await
    }

    fn release_lifecycle_guard(&mut self) {
        drop(self.lifecycle_guard.take());
    }

    fn disarm_request(&mut self) {
        self.request.take();
    }

    async fn rollback_rejection(&mut self) {
        if let Some(prior_terminal_sequence) = self.prior_terminal_sequence {
            self.store
                .rollback_user_stop_retention_for_attempt(
                    &self.session_id,
                    &self.attempt,
                    prior_terminal_sequence,
                )
                .await;
            self.prior_terminal_sequence = None;
        }
        self.disarm_request();
        self.release_lifecycle_guard();
    }

    async fn release_retention(&mut self) {
        if self.prior_terminal_sequence.is_some() {
            self.store
                .release_terminal_retention_hold_for_attempt(&self.session_id, &self.attempt)
                .await;
            self.prior_terminal_sequence = None;
        }
    }

    fn into_lease(mut self, record: LaunchSessionRecord) -> UserStopLease {
        self.disarm_request();
        self.prior_terminal_sequence.take();
        UserStopLease {
            store: self.store.clone(),
            lifecycle_guard: self.lifecycle_guard.take(),
            attempt: self.attempt.clone(),
            session_id: self.session_id.clone(),
            record,
            retention_hold_active: true,
        }
    }
}

impl Drop for PendingUserStop {
    fn drop(&mut self) {
        let Some(mut request) = self.request.take() else {
            return;
        };
        let store = self.store.clone();
        let session_id = self.session_id.clone();
        let attempt = self.attempt.clone();
        let lifecycle_guard = self.lifecycle_guard.take();
        let prior_terminal_sequence = self.prior_terminal_sequence.take();
        let acceptance_rejected = self.acceptance_rejected;
        tokio::spawn(async move {
            if acceptance_rejected {
                if let Some(prior_terminal_sequence) = prior_terminal_sequence {
                    store
                        .rollback_user_stop_retention_for_attempt(
                            &session_id,
                            &attempt,
                            prior_terminal_sequence,
                        )
                        .await;
                }
                drop(lifecycle_guard);
                return;
            }
            match request.accepted().await {
                Ok(acceptance) => {
                    if user_stop_was_accepted(acceptance) {
                        store
                            .record_user_stop_intent_for_attempt(&session_id, &attempt)
                            .await;
                    }
                    let _ = request.reaped().await;
                    if prior_terminal_sequence.is_some() {
                        store
                            .release_terminal_retention_hold_for_attempt(&session_id, &attempt)
                            .await;
                    }
                    drop(lifecycle_guard);
                }
                Err(_) => {
                    if let Some(prior_terminal_sequence) = prior_terminal_sequence {
                        store
                            .rollback_user_stop_retention_for_attempt(
                                &session_id,
                                &attempt,
                                prior_terminal_sequence,
                            )
                            .await;
                    }
                    drop(lifecycle_guard);
                }
            }
        });
    }
}

impl UserStopLease {
    pub(crate) fn record(&self) -> &LaunchSessionRecord {
        &self.record
    }

    pub(crate) async fn emit_log(&self, source: &'static str, text: impl Into<String>) {
        self.store
            .emit_log_for_attempt(
                &self.session_id,
                &self.attempt,
                source,
                text.into(),
                now_ms(),
            )
            .await;
    }

    pub(crate) async fn emit_status(&self, event: LaunchStatusEvent) {
        self.store
            .emit_status_for_attempt(&self.session_id, &self.attempt, event)
            .await;
    }

    pub(crate) async fn release(mut self) {
        self.release_hold().await;
        drop(self.lifecycle_guard.take());
    }

    async fn release_hold(&mut self) {
        if !self.retention_hold_active {
            return;
        }
        self.store
            .release_terminal_retention_hold_for_attempt(&self.session_id, &self.attempt)
            .await;
        self.retention_hold_active = false;
    }
}

impl Drop for UserStopLease {
    fn drop(&mut self) {
        let Some(lifecycle_guard) = self.lifecycle_guard.take() else {
            return;
        };
        if !self.retention_hold_active {
            drop(lifecycle_guard);
            return;
        }
        self.retention_hold_active = false;
        let store = self.store.clone();
        let session_id = self.session_id.clone();
        let attempt = self.attempt.clone();
        tokio::spawn(async move {
            store
                .release_terminal_retention_hold_for_attempt(&session_id, &attempt)
                .await;
            drop(lifecycle_guard);
        });
    }
}

impl SessionStore {
    pub fn new() -> Self {
        let (changes, _) = broadcast::channel(64);
        Self {
            sessions: RwLock::new(HashMap::new()),
            active_processes: Mutex::new(HashMap::new()),
            process_owner_changes: Notify::new(),
            lifecycle_transition: Arc::new(Mutex::new(())),
            shutdown_started: AtomicBool::new(false),
            changes,
            next_attempt_id: AtomicU64::new(0),
            next_terminal_sequence: AtomicU64::new(0),
        }
    }

    fn notify_changed(&self) {
        let _ = self.changes.send(());
    }

    fn next_attempt_id(&self) -> u64 {
        self.next_attempt_id
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .expect("process attempt id overflowed")
    }

    async fn current_process_attempt(&self, session_id: &str) -> Option<Arc<ProcessAttemptScope>> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.attempt.clone())
    }

    async fn acquire_launch_failure_terminalization_lease(
        self: &Arc<Self>,
        session_id: &str,
        attempt: Arc<ProcessAttemptScope>,
        lifecycle_guard: Option<OwnedMutexGuard<()>>,
    ) -> Result<LaunchFailureTerminalizationLease, LaunchFailureTerminationErrorClass> {
        let lifecycle_guard = match lifecycle_guard {
            Some(lifecycle_guard) => lifecycle_guard,
            None => self.lifecycle_transition.clone().lock_owned().await,
        };
        let current_attempt = self.current_process_attempt(session_id).await;
        if !current_attempt
            .as_ref()
            .is_some_and(|current| attempt_scopes_match(current, &attempt))
        {
            return Err(LaunchFailureTerminationErrorClass::StaleAttempt);
        }
        Ok(LaunchFailureTerminalizationLease {
            store: self.clone(),
            session_id: session_id.to_string(),
            attempt,
            lifecycle_guard: Some(lifecycle_guard),
        })
    }

    pub async fn insert(
        &self,
        mut record: LaunchSessionRecord,
    ) -> Result<(), SessionAdmissionError> {
        let _lifecycle_transition = self.lifecycle_transition.lock().await;
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(SessionAdmissionError);
        }
        let (events, _) = broadcast::channel(256);
        ensure_stage_started(&mut record, now_ms());
        let previous_process = self
            .sessions
            .read()
            .await
            .get(&record.session_id.0)
            .and_then(|entry| entry.process.clone());
        let mut sessions = self.sessions.write().await;
        sessions.insert(
            record.session_id.0.clone(),
            SessionEntry {
                attempt: ProcessAttemptScope::new(self.next_attempt_id()),
                process: None,
                record,
                events,
                observed_failure: None,
                log_count: 0,
                stop_requested: false,
                retention_holds: 1,
                terminal_sequence: None,
            },
        );
        drop(sessions);
        if let Some(previous_process) = previous_process {
            let _ = previous_process.terminate(supervisor::ProcessTerminalCause::Replacement);
        }
        self.notify_changed();
        Ok(())
    }

    pub async fn get(&self, session_id: &str) -> Option<LaunchSessionRecord> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.record.clone())
    }

    pub(super) async fn process_exit_context(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
        exit_observed_at_ms: u64,
    ) -> Option<ProcessExitContext> {
        let sessions = self.sessions.read().await;
        let entry = sessions
            .get(session_id)
            .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))?;
        Some(ProcessExitContext {
            record: entry.record.clone(),
            observed_failure: entry
                .observed_failure
                .and_then(|signal| signal.fresh_for_exit(exit_observed_at_ms)),
        })
    }

    async fn startup_watchdog_process_for_attempt(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
    ) -> Option<supervisor::ProcessControlHandle> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))
            .filter(|entry| {
                !entry.stop_requested
                    && matches!(
                        entry.record.state,
                        LaunchState::Starting | LaunchState::Monitoring
                    )
                    && entry.record.boot_completed_at_ms.is_none()
            })
            .and_then(|entry| entry.process.clone())
    }

    pub(super) async fn process_owner_completed(&self, attempt_id: u64) {
        self.active_processes.lock().await.remove(&attempt_id);
        self.process_owner_changes.notify_waiters();
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

    pub async fn acquire_terminal_retention_hold(
        &self,
        session_id: &str,
    ) -> Option<LaunchSessionRecord> {
        let mut sessions = self.sessions.write().await;
        let entry = sessions.get_mut(session_id)?;
        entry.retention_holds = entry
            .retention_holds
            .checked_add(1)
            .expect("session retention hold count overflowed");
        entry.terminal_sequence = None;
        Some(entry.record.clone())
    }

    pub async fn release_terminal_retention_hold(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        let Some(entry) = sessions.get_mut(session_id) else {
            return;
        };
        entry.retention_holds = entry
            .retention_holds
            .checked_sub(1)
            .expect("released a session retention hold that was not acquired");
        if entry.retention_holds == 0
            && classify::is_terminal_state(entry.record.state)
            && entry.terminal_sequence.is_none()
        {
            entry.terminal_sequence =
                Some(self.next_terminal_sequence.fetch_add(1, Ordering::Relaxed));
        }
        let evicted = evict_oldest_terminal_sessions(&mut sessions);
        drop(sessions);
        if evicted {
            self.notify_changed();
        }
    }

    async fn release_terminal_retention_hold_for_attempt(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
    ) {
        let mut sessions = self.sessions.write().await;
        let Some(entry) = sessions
            .get_mut(session_id)
            .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))
        else {
            return;
        };
        entry.retention_holds = entry
            .retention_holds
            .checked_sub(1)
            .expect("released a session retention hold that was not acquired");
        if entry.retention_holds == 0
            && classify::is_terminal_state(entry.record.state)
            && entry.terminal_sequence.is_none()
        {
            entry.terminal_sequence =
                Some(self.next_terminal_sequence.fetch_add(1, Ordering::Relaxed));
        }
        let evicted = evict_oldest_terminal_sessions(&mut sessions);
        drop(sessions);
        if evicted {
            self.notify_changed();
        }
    }

    async fn rollback_user_stop_retention_for_attempt(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
        prior_terminal_sequence: Option<u64>,
    ) {
        let mut sessions = self.sessions.write().await;
        let Some(entry) = sessions
            .get_mut(session_id)
            .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))
        else {
            return;
        };
        entry.retention_holds = entry
            .retention_holds
            .checked_sub(1)
            .expect("released a user-stop retention hold that was not acquired");
        if entry.terminal_sequence.is_none() {
            entry.terminal_sequence = prior_terminal_sequence;
        }
    }

    async fn record_user_stop_intent_for_attempt(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
    ) -> Option<LaunchSessionRecord> {
        let mut sessions = self.sessions.write().await;
        let entry = sessions
            .get_mut(session_id)
            .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))?;
        record_user_stop_intent(entry, session_id);
        Some(entry.record.clone())
    }

    pub async fn emit_log(
        &self,
        session_id: &str,
        source: impl Into<String>,
        text: impl Into<String>,
    ) {
        let Some(attempt) = self.current_process_attempt(session_id).await else {
            return;
        };
        self.emit_log_for_attempt_with(
            session_id,
            &attempt,
            RawLogLine {
                source: source.into(),
                text: text.into(),
                observed_at_ms: now_ms(),
            },
            prepare_log_line,
            |process| async move {
                match process {
                    Some(process) => process.promote_after_boot().await,
                    None => supervisor::ProcessPriorityReply::Completed(
                        priority::promote_after_boot(None).map(|promotion| promotion.proof_value()),
                    ),
                }
            },
        )
        .await;
    }

    pub(super) async fn emit_log_for_attempt(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
        source: &'static str,
        text: String,
        observed_at_ms: u64,
    ) {
        self.emit_log_for_attempt_with(
            session_id,
            attempt,
            RawLogLine {
                source: source.to_string(),
                text,
                observed_at_ms,
            },
            prepare_log_line,
            |process| async move {
                match process {
                    Some(process) => process.promote_after_boot().await,
                    None => supervisor::ProcessPriorityReply::Completed(
                        priority::promote_after_boot(None).map(|promotion| promotion.proof_value()),
                    ),
                }
            },
        )
        .await;
    }

    async fn emit_log_for_attempt_with<Prepare, Promote, PromoteFuture>(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
        line: RawLogLine,
        prepare: Prepare,
        promote_after_boot: Promote,
    ) where
        Prepare: FnOnce(&str, String, String, u64) -> PreparedLogLine,
        Promote: FnOnce(Option<supervisor::ProcessControlHandle>) -> PromoteFuture,
        PromoteFuture: Future<Output = supervisor::ProcessPriorityReply>,
    {
        let _log_transition_guard = attempt.log_transition.lock().await;
        let prepared = prepare(session_id, line.source, line.text, line.observed_at_ms);
        if prepared.boot_evidence.is_none() {
            let mut sessions = self.sessions.write().await;
            if let Some(entry) = sessions
                .get_mut(session_id)
                .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))
            {
                observe_log(entry);
                publish_prepared_log(entry, prepared);
            }
            return;
        }

        let (ticket, gate) = {
            let sessions = self.sessions.read().await;
            let Some(entry) = sessions
                .get(session_id)
                .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))
            else {
                return;
            };
            let ticket = BootPriorityPromotionTicket {
                attempt: entry.attempt.clone(),
                process: entry.process.clone(),
                pid: entry.record.pid,
            };
            let gate = boot_promotion_gate(Some(entry), &ticket);
            (ticket, gate)
        };
        if matches!(gate, BootPromotionGate::Stale) {
            return;
        }

        self.apply_boot_priority_promotion(session_id, ticket, prepared, gate, promote_after_boot)
            .await;
    }

    async fn apply_boot_priority_promotion<Promote, PromoteFuture>(
        &self,
        session_id: &str,
        ticket: BootPriorityPromotionTicket,
        prepared: PreparedLogLine,
        pre_effect_gate: BootPromotionGate,
        promote_after_boot: Promote,
    ) where
        Promote: FnOnce(Option<supervisor::ProcessControlHandle>) -> PromoteFuture,
        PromoteFuture: Future<Output = supervisor::ProcessPriorityReply>,
    {
        let mut gate = pre_effect_gate;
        let mut raw_error = None;
        let mut promotion_error = None;
        let promotion = match gate {
            BootPromotionGate::CompleteSkipped(proof) => proof,
            BootPromotionGate::Promote => match promote_after_boot(ticket.process.clone()).await {
                supervisor::ProcessPriorityReply::Completed(result) => match result {
                    Ok(proof) => proof,
                    Err(error) => {
                        promotion_error = priority::sanitize_priority_error(&error);
                        raw_error = Some(error);
                        "failed"
                    }
                },
                supervisor::ProcessPriorityReply::ExitedBefore => {
                    gate = BootPromotionGate::CompleteSkipped("skipped_process_already_exited");
                    "skipped_process_already_exited"
                }
                supervisor::ProcessPriorityReply::ExitedAfter => {
                    gate = BootPromotionGate::CompleteSkipped(
                        "skipped_process_exited_during_promotion",
                    );
                    "skipped_process_exited_during_promotion"
                }
                supervisor::ProcessPriorityReply::StateUnavailable => {
                    gate = BootPromotionGate::CompleteSkipped("skipped_process_state_unavailable");
                    "skipped_process_state_unavailable"
                }
                supervisor::ProcessPriorityReply::StopAccepted => {
                    gate = BootPromotionGate::CompleteSkipped("skipped_stop_requested");
                    "skipped_stop_requested"
                }
            },
            BootPromotionGate::Resolve => "skipped",
            BootPromotionGate::Stale => return,
        };
        let published = self
            .settle_boot_promotion(
                session_id,
                &ticket,
                prepared,
                gate,
                promotion,
                promotion_error,
            )
            .await;

        if let Some(error) = raw_error {
            tracing::warn!(
                session_id,
                pid = ticket.pid,
                error = %error,
                "failed to promote launched game process after boot marker"
            );
        }
        if published {
            self.notify_changed();
        }
    }

    async fn settle_boot_promotion(
        &self,
        session_id: &str,
        ticket: &BootPriorityPromotionTicket,
        prepared: PreparedLogLine,
        pre_effect_gate: BootPromotionGate,
        promotion: &'static str,
        promotion_error: Option<String>,
    ) -> bool {
        let mut sessions = self.sessions.write().await;
        let Some(entry) = sessions.get_mut(session_id) else {
            return false;
        };
        let final_gate = boot_promotion_gate(Some(entry), ticket);
        let gate = match final_gate {
            BootPromotionGate::Promote => pre_effect_gate,
            final_gate => final_gate,
        };
        if matches!(gate, BootPromotionGate::Stale) {
            return false;
        }
        if matches!(gate, BootPromotionGate::Resolve) {
            observe_log(entry);
            publish_prepared_log(entry, prepared);
            return false;
        }

        let promotion = match gate {
            BootPromotionGate::CompleteSkipped("skipped_stop_requested")
                if matches!(pre_effect_gate, BootPromotionGate::Promote) =>
            {
                promotion
            }
            BootPromotionGate::CompleteSkipped(proof) => proof,
            BootPromotionGate::Promote => promotion,
            BootPromotionGate::Stale | BootPromotionGate::Resolve => unreachable!(),
        };
        observe_log(entry);
        entry.observed_failure = None;
        record_priority_promotion(entry, promotion, promotion_error);
        complete_boot(entry, prepared.observed_at_ms);
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
            evidence: prepared.boot_evidence.clone().unwrap_or_default(),
            stages: Vec::new(),
        };
        apply_status_update(entry, &mut status);
        let _ = entry.events.send(LaunchEvent::Status(Box::new(status)));
        publish_prepared_log(entry, prepared);
        true
    }

    pub async fn emit_status(&self, session_id: &str, event: LaunchStatusEvent) {
        let Some(attempt) = self.current_process_attempt(session_id).await else {
            return;
        };
        self.emit_status_for_attempt(session_id, &attempt, event)
            .await;
    }

    pub(super) async fn emit_status_for_attempt(
        &self,
        session_id: &str,
        attempt: &Arc<ProcessAttemptScope>,
        mut event: LaunchStatusEvent,
    ) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions
            .get_mut(session_id)
            .filter(|entry| attempt_scopes_match(&entry.attempt, attempt))
        {
            let next_state = classify::parse_launch_state(&event.state);
            if process_state_regresses(entry.record.state, next_state) {
                return;
            }
            apply_status_update(entry, &mut event);
            if entry.retention_holds == 0 && classify::is_terminal_state(entry.record.state) {
                if entry.terminal_sequence.is_none() {
                    entry.terminal_sequence =
                        Some(self.next_terminal_sequence.fetch_add(1, Ordering::Relaxed));
                }
            } else {
                entry.terminal_sequence = None;
            }
            let _ = entry.events.send(LaunchEvent::Status(Box::new(event)));
            evict_oldest_terminal_sessions(&mut sessions);
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
        let _lifecycle_transition = self.lifecycle_transition.lock().await;
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(session_shutdown_error());
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.kill_on_drop(true);
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
        let session_id = record.session_id.0.clone();
        let previous_process = self
            .sessions
            .read()
            .await
            .get(&session_id)
            .and_then(|entry| entry.process.clone());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let mut sessions = self.sessions.write().await;
                if let Some(entry) = sessions.get_mut(&session_id) {
                    entry.record.priority = Some(priority);
                }
                return Err(error);
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process_started_at_ms = now_ms();
        record.pid = child.id();
        record.process_started_at_ms = Some(process_started_at_ms);
        record.boot_completed_at_ms = None;
        record.boot_duration_ms = None;
        record.state = LaunchState::Starting;
        record.failure = None;
        record.outcome = None;
        let (process, owner) = supervisor::prepare_process_owner(child);
        let attempt = ProcessAttemptScope::new(self.next_attempt_id());

        // Registration always acquires the active-owner registry before the session map. No
        // await occurs after both guards are held, so the prepared kill-on-drop child is either
        // wholly unregistered or synchronously installed in both places before its owner runs.
        let mut active_processes = self.active_processes.lock().await;
        let mut sessions = self.sessions.write().await;
        let (events, retention_holds) = if let Some(entry) = sessions.get(&session_id) {
            (entry.events.clone(), entry.retention_holds)
        } else {
            let (events, _) = broadcast::channel(256);
            (events, 1)
        };
        let mut stored_record = record.clone();
        if let Some(previous) = sessions.get(&session_id) {
            stored_record.state = previous.record.state;
            stored_record.stages = previous.record.stages.clone();
            if stored_record.benchmark.is_none() {
                stored_record.benchmark = previous.record.benchmark.clone();
            }
        }
        ensure_stage_started(&mut stored_record, now_ms());
        let mut starting_status = LaunchStatusEvent {
            state: "starting".to_string(),
            benchmark: stored_record.benchmark.clone(),
            pid: record.pid,
            exit_code: None,
            failure_class: None,
            failure_detail: None,
            healing: stored_record.healing.clone(),
            guardian: stored_record.guardian.clone(),
            outcome: None,
            notice: None,
            evidence: Vec::new(),
            stages: Vec::new(),
        };
        let mut entry = SessionEntry {
            attempt: attempt.clone(),
            process: Some(process.clone()),
            record: stored_record,
            events,
            observed_failure: None,
            log_count: 0,
            stop_requested: false,
            retention_holds,
            terminal_sequence: None,
        };
        apply_status_update(&mut entry, &mut starting_status);
        let _ = entry
            .events
            .send(LaunchEvent::Status(Box::new(starting_status)));
        sessions.insert(session_id.clone(), entry);
        active_processes.insert(attempt.id, process);
        let output_pumps = supervisor::spawn_output_tasks(
            self.clone(),
            session_id.clone(),
            attempt.clone(),
            stdout,
            stderr,
        );
        owner.spawn(
            self.clone(),
            session_id.clone(),
            attempt.clone(),
            output_pumps,
        );
        drop(sessions);
        drop(active_processes);
        if let Some(previous_process) = previous_process {
            let _ = previous_process.terminate(supervisor::ProcessTerminalCause::Replacement);
        }
        self.notify_changed();

        supervisor::spawn_startup_watchdog(self.clone(), session_id.clone(), attempt.clone());

        Ok(record)
    }

    pub async fn kill(self: &Arc<Self>, session_id: &str) -> Result<(), SessionStopError> {
        let lifecycle_guard = self.lifecycle_transition.clone().lock_owned().await;
        let (attempt, process) = {
            let sessions = self.sessions.read().await;
            let Some(entry) = sessions.get(session_id) else {
                return Err(SessionStopError::SessionNotFound);
            };
            let Some(process) = entry.process.clone() else {
                return Err(SessionStopError::NoLiveProcess);
            };
            (entry.attempt.clone(), process)
        };
        let mut stop = PendingUserStop {
            store: self.clone(),
            session_id: session_id.to_string(),
            attempt,
            request: Some(process.terminate(supervisor::ProcessTerminalCause::UserStop)),
            lifecycle_guard: Some(lifecycle_guard),
            prior_terminal_sequence: None,
            acceptance_rejected: false,
        };
        let acceptance = match stop.accepted().await {
            Ok(acceptance) => acceptance,
            Err(error) => {
                stop.rollback_rejection().await;
                return Err(SessionStopError::Process(error));
            }
        };
        if !user_stop_was_accepted(acceptance) {
            stop.release_lifecycle_guard();
            let _ = stop.reaped().await;
            stop.disarm_request();
            return Err(SessionStopError::NoLiveProcess);
        }
        stop.publish_intent(acceptance).await;
        stop.release_lifecycle_guard();
        let reaped = stop.reaped().await;
        stop.disarm_request();
        reaped.map(|_| ()).map_err(SessionStopError::Process)
    }

    pub(crate) async fn terminate_for_launch_failure(
        self: &Arc<Self>,
        session_id: &str,
    ) -> LaunchFailureTermination {
        let lifecycle_guard = self.lifecycle_transition.clone().lock_owned().await;
        let Some((attempt, process, pid)) =
            self.sessions.read().await.get(session_id).map(|entry| {
                (
                    entry.attempt.clone(),
                    entry.process.clone(),
                    entry.record.pid,
                )
            })
        else {
            return LaunchFailureTermination::Unconfirmed(
                LaunchFailureTerminationErrorClass::MissingProcess,
            );
        };
        let Some(process) = process else {
            if pid.is_some() {
                return LaunchFailureTermination::Unconfirmed(
                    LaunchFailureTerminationErrorClass::MissingProcess,
                );
            }
            return match self
                .acquire_launch_failure_terminalization_lease(
                    session_id,
                    attempt,
                    Some(lifecycle_guard),
                )
                .await
            {
                Ok(lease) => LaunchFailureTermination::Ready(lease),
                Err(error_class) => LaunchFailureTermination::Unconfirmed(error_class),
            };
        };

        let mut request = process.terminate(supervisor::ProcessTerminalCause::LaunchFailure);
        if request.accepted().await.is_err() {
            if request.terminal_is_settled() {
                return match self
                    .acquire_launch_failure_terminalization_lease(
                        session_id,
                        attempt,
                        Some(lifecycle_guard),
                    )
                    .await
                {
                    Ok(lease) => LaunchFailureTermination::Ready(lease),
                    Err(error_class) => LaunchFailureTermination::Unconfirmed(error_class),
                };
            }

            let owner_active = self.active_processes.lock().await.contains_key(&attempt.id);
            let attempt_current = self
                .sessions
                .read()
                .await
                .get(session_id)
                .is_some_and(|entry| attempt_scopes_match(&entry.attempt, &attempt));
            if request.terminal_is_settled() && attempt_current {
                return match self
                    .acquire_launch_failure_terminalization_lease(
                        session_id,
                        attempt,
                        Some(lifecycle_guard),
                    )
                    .await
                {
                    Ok(lease) => LaunchFailureTermination::Ready(lease),
                    Err(error_class) => LaunchFailureTermination::Unconfirmed(error_class),
                };
            }
            drop(lifecycle_guard);
            if owner_active && attempt_current {
                return LaunchFailureTermination::Pending(PendingLaunchFailureTermination {
                    store: self.clone(),
                    session_id: session_id.to_string(),
                    attempt,
                    request,
                });
            }
            return LaunchFailureTermination::Unconfirmed(
                LaunchFailureTerminationErrorClass::OwnerUnavailable,
            );
        }

        drop(lifecycle_guard);
        let settled = request.settled().await;
        if settled.is_err() && !request.terminal_is_settled() {
            return LaunchFailureTermination::Unconfirmed(
                LaunchFailureTerminationErrorClass::SettlementUnavailable,
            );
        }
        match self
            .acquire_launch_failure_terminalization_lease(session_id, attempt, None)
            .await
        {
            Ok(lease) => LaunchFailureTermination::Ready(lease),
            Err(error_class) => LaunchFailureTermination::Unconfirmed(error_class),
        }
    }

    pub(crate) async fn begin_user_stop(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<UserStopLease, SessionStopError> {
        let lifecycle_guard = self.lifecycle_transition.clone().lock_owned().await;
        let (attempt, process, prior_terminal_sequence) = {
            let mut sessions = self.sessions.write().await;
            let Some(entry) = sessions.get_mut(session_id) else {
                return Err(SessionStopError::SessionNotFound);
            };
            let Some(process) = entry.process.clone() else {
                return Err(SessionStopError::NoLiveProcess);
            };
            entry.retention_holds = entry
                .retention_holds
                .checked_add(1)
                .expect("session retention hold count overflowed");
            let prior_terminal_sequence = entry.terminal_sequence;
            entry.terminal_sequence = None;
            (entry.attempt.clone(), process, prior_terminal_sequence)
        };
        let mut stop = PendingUserStop {
            store: self.clone(),
            session_id: session_id.to_string(),
            attempt,
            request: Some(process.terminate(supervisor::ProcessTerminalCause::UserStop)),
            lifecycle_guard: Some(lifecycle_guard),
            prior_terminal_sequence: Some(prior_terminal_sequence),
            acceptance_rejected: false,
        };
        let acceptance = match stop.accepted().await {
            Ok(acceptance) => acceptance,
            Err(error) => {
                stop.rollback_rejection().await;
                return Err(SessionStopError::Process(error));
            }
        };
        let Some(record) = stop.publish_intent(acceptance).await else {
            stop.release_lifecycle_guard();
            let _ = stop.reaped().await;
            stop.release_retention().await;
            stop.disarm_request();
            return Err(SessionStopError::NoLiveProcess);
        };
        if let Err(error) = stop.reaped().await {
            stop.release_retention().await;
            stop.disarm_request();
            stop.release_lifecycle_guard();
            return Err(SessionStopError::Process(error));
        }
        Ok(stop.into_lease(record))
    }

    #[cfg(test)]
    pub async fn observed_failure(&self, session_id: &str) -> Option<LaunchFailureClass> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.observed_failure.map(|signal| signal.class))
    }

    #[cfg(test)]
    pub(crate) async fn reject_next_process_start_kill(&self, session_id: &str) -> bool {
        let process = self
            .sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.process.clone());
        process.is_some_and(|process| {
            process.reject_next_start_kill();
            true
        })
    }

    #[cfg(test)]
    pub(crate) async fn retention_hold_count(&self, session_id: &str) -> Option<usize> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.retention_holds)
    }

    pub async fn terminate_all(self: &Arc<Self>) -> std::io::Result<()> {
        self.shutdown_started.store(true, Ordering::Release);
        let store = self.clone();
        tokio::spawn(async move {
            let lifecycle_transition = store.lifecycle_transition.clone().lock_owned().await;
            store.coordinate_terminate_all(lifecycle_transition).await
        })
        .await
        .map_err(|_| std::io::Error::other("launch process shutdown coordinator stopped"))?
    }

    async fn coordinate_terminate_all(
        self: &Arc<Self>,
        _lifecycle_transition: OwnedMutexGuard<()>,
    ) -> std::io::Result<()> {
        let processes = self
            .active_processes
            .lock()
            .await
            .iter()
            .map(|(attempt_id, process)| (*attempt_id, process.clone()))
            .collect::<Vec<_>>();
        let mut requests = processes
            .into_iter()
            .map(|(attempt_id, process)| {
                (
                    attempt_id,
                    process.terminate(supervisor::ProcessTerminalCause::Shutdown),
                )
            })
            .collect::<Vec<_>>();

        let mut first_error = None;
        let mut settled_owner_ids = Vec::new();
        for (attempt_id, request) in &mut requests {
            let result = request.settled().await;
            if request.terminal_is_settled() {
                settled_owner_ids.push(*attempt_id);
            }
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(bounded_shutdown_error(&error));
            }
        }
        for attempt_id in settled_owner_ids {
            self.wait_for_process_owner_removal(attempt_id).await;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        if !self.active_processes.lock().await.is_empty() {
            return Err(std::io::Error::other(
                "launch process shutdown is incomplete",
            ));
        }

        self.sessions.write().await.clear();
        self.notify_changed();
        Ok(())
    }

    async fn wait_for_process_owner_removal(&self, attempt_id: u64) {
        loop {
            let removed = self.process_owner_changes.notified();
            tokio::pin!(removed);
            removed.as_mut().enable();
            if !self.active_processes.lock().await.contains_key(&attempt_id) {
                return;
            }
            removed.await;
        }
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
                    entry.record.boot_completed_at_ms.is_some(),
                    entry.log_count,
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

fn session_shutdown_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "launch session store is shutting down",
    )
}

fn bounded_shutdown_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), "launch process shutdown is incomplete")
}

fn evict_oldest_terminal_sessions(sessions: &mut HashMap<String, SessionEntry>) -> bool {
    let terminal_count = sessions
        .values()
        .filter(|entry| entry.terminal_sequence.is_some())
        .count();
    let excess = terminal_count.saturating_sub(MAX_RETAINED_TERMINAL_SESSIONS);
    if excess == 0 {
        return false;
    }

    let mut terminal_sessions = sessions
        .iter()
        .filter_map(|(session_id, entry)| {
            entry
                .terminal_sequence
                .map(|sequence| (sequence, session_id.clone()))
        })
        .collect::<Vec<_>>();
    terminal_sessions.sort_unstable();

    for (_, session_id) in terminal_sessions.into_iter().take(excess) {
        sessions.remove(&session_id);
    }
    true
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
                boot_completed: entry.record.boot_completed_at_ms.is_some(),
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

fn prepare_log_line(
    session_id: &str,
    source: String,
    text: String,
    observed_at_ms: u64,
) -> PreparedLogLine {
    let boot_evidence = classify::boot_marker_detected(&text).then(|| {
        process_observation_stage_evidence(
            session_id,
            ProcessObservation::BootEvidence {
                label: "boot_marker",
            },
        )
    });
    let failure_class = match classify_startup_failure_text(&text) {
        LaunchFailureClass::Unknown => None,
        failure_class => Some(failure_class),
    };
    let source = crate::observability::sanitize_evidence_token(
        &source,
        crate::observability::RedactionAudience::UserVisible,
        32,
    )
    .unwrap_or_else(|| "game".to_string());
    let text = crate::observability::sanitize_public_log_line(
        &text,
        crate::observability::RedactionAudience::UserVisible,
        MAX_LAUNCH_LOG_LINE_CHARS,
    );

    PreparedLogLine {
        observed_at_ms,
        boot_evidence,
        failure_class,
        event: LaunchLogEvent { source, text },
    }
}

fn complete_boot(entry: &mut SessionEntry, now: u64) {
    entry.record.boot_completed_at_ms = Some(now);
    entry.record.boot_duration_ms = entry
        .record
        .process_started_at_ms
        .map(|started_at| now.saturating_sub(started_at));
}

fn observe_log(entry: &mut SessionEntry) {
    entry.log_count += 1;
}

fn publish_prepared_log(entry: &mut SessionEntry, prepared: PreparedLogLine) {
    if let Some(failure_class) = prepared.failure_class {
        entry.observed_failure = Some(ObservedFailureSignal {
            class: failure_class,
            observed_at_ms: prepared.observed_at_ms,
        });
    }
    let _ = entry.events.send(LaunchEvent::Log(prepared.event));
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

fn process_observation_stage_evidence(
    session_id: &str,
    observation: ProcessObservation<'_>,
) -> Vec<LaunchStageEvidence> {
    let report = observe_process(ProcessObservationRequest::new(
        process_session_target(session_id),
        observation,
    ));
    process_stage_evidence(&report.facts)
}

fn process_kill_stage_evidence(
    session_id: &str,
    reason: ProcessKillReason,
) -> Vec<LaunchStageEvidence> {
    let report = process_killed(ProcessKillRequest::new(
        process_session_target(session_id),
        reason,
    ));
    process_stage_evidence(&report.facts)
}

fn process_stop_stage_evidence(
    session_id: &str,
    intent: ProcessStopIntent,
) -> Vec<LaunchStageEvidence> {
    let report = process_stop_requested(ProcessStopRequest::new(
        process_session_target(session_id),
        intent,
    ));
    process_stage_evidence(&report.facts)
}

fn record_user_stop_intent(entry: &mut SessionEntry, session_id: &str) {
    if entry.stop_requested {
        return;
    }
    entry.stop_requested = true;
    let evidence = process_stop_stage_evidence(session_id, ProcessStopIntent::UserRequested);
    ensure_stage_started(&mut entry.record, now_ms());
    apply_stage_evidence(entry.record.stages.last_mut(), &evidence);
}

fn user_stop_was_accepted(acceptance: supervisor::ProcessTerminationAcceptance) -> bool {
    matches!(
        acceptance,
        supervisor::ProcessTerminationAcceptance::Accepted(
            supervisor::ProcessTerminalCause::UserStop
        ) | supervisor::ProcessTerminationAcceptance::Joined(
            supervisor::ProcessTerminalCause::UserStop
        )
    )
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
    crate::observability::sanitize_public_json_value(
        value,
        crate::observability::RedactionAudience::UserVisible,
        MAX_NOTICE_DETAIL_CHARS,
        64,
    )
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

pub(super) fn now_ms() -> u64 {
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
fn test_record(session_id: &str) -> LaunchSessionRecord {
    LaunchSessionRecord {
        session_id: axial_launcher::SessionId(session_id.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axial_launcher::{LaunchSessionExitReason, LaunchStageEvidence};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::process::Command;

    #[tokio::test]
    async fn launch_terminal_session_retention_bounds_repeated_transitions_and_keeps_active() {
        let store = SessionStore::new();
        store
            .insert(test_record("active-session"))
            .await
            .expect("insert session");

        let overflow = 5;
        for index in 0..MAX_RETAINED_TERMINAL_SESSIONS + overflow {
            let session_id = format!("terminal-{index}");
            store
                .insert(test_record(&session_id))
                .await
                .expect("insert session");
            store.release_terminal_retention_hold(&session_id).await;
            store
                .emit_status(&session_id, terminal_status(Some(0), None, None))
                .await;
        }

        let sessions = store.sessions.read().await;
        assert_eq!(sessions.len(), MAX_RETAINED_TERMINAL_SESSIONS + 1);
        assert!(sessions.contains_key("active-session"));
        assert_eq!(
            sessions
                .values()
                .filter(|entry| entry.terminal_sequence.is_some())
                .count(),
            MAX_RETAINED_TERMINAL_SESSIONS
        );
        for index in 0..overflow {
            assert!(!sessions.contains_key(&format!("terminal-{index}")));
        }
        for index in overflow..MAX_RETAINED_TERMINAL_SESSIONS + overflow {
            assert!(sessions.contains_key(&format!("terminal-{index}")));
        }
    }

    #[tokio::test]
    async fn launch_retained_terminal_session_replays_the_broadcast_status_snapshot() {
        let store = SessionStore::new();
        let retained_session_id = format!("terminal-{MAX_RETAINED_TERMINAL_SESSIONS}");
        let mut retained_receiver = None;

        for index in 0..=MAX_RETAINED_TERMINAL_SESSIONS {
            let session_id = format!("terminal-{index}");
            store
                .insert(test_record(&session_id))
                .await
                .expect("insert session");
            if session_id == retained_session_id {
                retained_receiver = store.subscribe(&session_id).await;
            }
            store.release_terminal_retention_hold(&session_id).await;
            store
                .emit_status(&session_id, terminal_status(Some(index as i32), None, None))
                .await;
        }

        assert!(store.get("terminal-0").await.is_none());
        let emitted = recv_status(&mut retained_receiver.expect("retained receiver")).await;
        let retained = store
            .get(&retained_session_id)
            .await
            .expect("retained terminal record");
        let replay = axial_launcher::snapshot_status(&retained);

        assert_eq!(replay.state, emitted.state);
        assert_eq!(replay.exit_code, emitted.exit_code);
        assert_eq!(replay.outcome, emitted.outcome);
        assert_eq!(replay.notice, emitted.notice);
        assert_eq!(replay.stages, emitted.stages);
        assert!(store.subscribe(&retained_session_id).await.is_some());
    }

    #[tokio::test]
    async fn launch_retry_pending_terminal_survives_pressure_and_resumes_original_stream() {
        let store = SessionStore::new();
        let retry_session_id = "retry-pending";
        store
            .insert(test_record(retry_session_id))
            .await
            .expect("insert session");
        let mut receiver = store
            .subscribe(retry_session_id)
            .await
            .expect("retry receiver");
        store
            .emit_status(
                retry_session_id,
                terminal_status(Some(1), Some("unknown"), None),
            )
            .await;

        insert_retention_ready_terminal_burst(&store, "completed").await;

        assert!(store.get(retry_session_id).await.is_some());
        let mut resumed = terminal_status(None, None, None);
        resumed.state = "preparing".to_string();
        store.emit_status(retry_session_id, resumed).await;

        assert_eq!(recv_status(&mut receiver).await.state, "exited");
        assert_eq!(recv_status(&mut receiver).await.state, "preparing");
    }

    #[tokio::test]
    async fn launch_nested_retention_holds_require_the_final_release_for_eligibility() {
        let store = SessionStore::new();
        let session_id = "nested-holds";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        assert!(
            store
                .acquire_terminal_retention_hold(session_id)
                .await
                .is_some()
        );
        store
            .emit_status(session_id, terminal_status(Some(0), None, None))
            .await;

        store.release_terminal_retention_hold(session_id).await;
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("held session")
                .terminal_sequence,
            None
        );

        store.release_terminal_retention_hold(session_id).await;
        assert!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("eligible session")
                .terminal_sequence
                .is_some()
        );
    }

    #[tokio::test]
    async fn launch_proof_pending_terminal_survives_pressure_until_its_hold_is_released() {
        let store = SessionStore::new();
        let proof_session_id = "proof-pending";
        store
            .insert(test_record(proof_session_id))
            .await
            .expect("insert session");
        store
            .emit_status(
                proof_session_id,
                terminal_status(Some(-9), None, Some("stopped by user")),
            )
            .await;

        insert_retention_ready_terminal_burst(&store, "completed").await;

        let proof_record = store
            .get(proof_session_id)
            .await
            .expect("proof source record must remain available");
        assert_eq!(proof_record.state, LaunchState::Exited);
        assert_eq!(proof_record.exit_code, Some(-9));

        store
            .release_terminal_retention_hold(proof_session_id)
            .await;
        assert!(store.get(proof_session_id).await.is_some());
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .values()
                .filter(|entry| entry.terminal_sequence.is_some())
                .count(),
            MAX_RETAINED_TERMINAL_SESSIONS
        );
    }

    async fn insert_retention_ready_terminal_burst(store: &SessionStore, prefix: &str) {
        for index in 0..=MAX_RETAINED_TERMINAL_SESSIONS {
            let session_id = format!("{prefix}-{index}");
            store
                .insert(test_record(&session_id))
                .await
                .expect("insert session");
            store.release_terminal_retention_hold(&session_id).await;
            store
                .emit_status(&session_id, terminal_status(Some(0), None, None))
                .await;
        }
    }

    #[tokio::test]
    async fn launch_stage_history_tracks_transitions_results_and_healing_notes() {
        let store = SessionStore::new();
        store
            .insert(test_record("stage-history"))
            .await
            .expect("insert session");

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
        store
            .insert(test_record("guardian-stage-notes"))
            .await
            .expect("insert session");

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
        store
            .insert(test_record("guardian-stage-redaction"))
            .await
            .expect("insert session");

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
        store
            .insert(test_record("stage-evidence"))
            .await
            .expect("insert session");

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

        store
            .insert(test_record("guardian-allowed"))
            .await
            .expect("insert session");
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

        store
            .insert(test_record("guardian-empty"))
            .await
            .expect("insert session");
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

        store
            .insert(test_record("guardian-malformed"))
            .await
            .expect("insert session");
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
        store
            .insert(test_record("benchmark-status"))
            .await
            .expect("insert session");
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

    #[tokio::test]
    async fn kill_distinguishes_missing_session_from_session_without_live_process() {
        let store = Arc::new(SessionStore::new());

        assert!(matches!(
            store.kill("missing-session").await,
            Err(SessionStopError::SessionNotFound)
        ));

        let session_id = "session-without-process";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        assert!(matches!(
            store.kill(session_id).await,
            Err(SessionStopError::NoLiveProcess)
        ));
        let entry = store.sessions.read().await;
        assert!(!entry.get(session_id).expect("session entry").stop_requested);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_kill_preserves_live_attempt_watchdog_state() {
        let store = Arc::new(SessionStore::new());
        let session_id = "rejected-user-kill";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("start kill target");
        let before = store.get(session_id).await.expect("record before kill");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("current attempt");
        assert!(store.reject_next_process_start_kill(session_id).await);

        let error = store
            .kill(session_id)
            .await
            .expect_err("injected kill rejection");

        assert!(matches!(
            error,
            SessionStopError::Process(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        {
            let sessions = store.sessions.read().await;
            let entry = sessions.get(session_id).expect("live session");
            assert!(!entry.stop_requested);
            assert_eq!(entry.record.stages, before.stages);
        }
        assert!(
            store
                .startup_watchdog_process_for_attempt(session_id, &attempt)
                .await
                .is_some()
        );
        store.terminate_all().await.expect("terminate kill target");
    }

    #[tokio::test]
    async fn cancelled_kill_pending_acceptance_preserves_accepted_user_stop() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-kill-acceptance";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let mut gated = supervisor::gated_termination_control();
        store
            .sessions
            .write()
            .await
            .get_mut(session_id)
            .expect("session")
            .process = Some(gated.handle.clone());
        let kill_store = store.clone();
        let kill = tokio::spawn(async move { kill_store.kill(session_id).await });
        gated.capture_user_stop_request().await;

        kill.abort();
        assert!(kill.await.expect_err("cancelled kill").is_cancelled());
        gated.accept_user_stop();
        wait_for_user_stop_intent(&store, session_id).await;
        assert_eq!(user_stop_evidence_count(&store, session_id).await, 1);
        store
            .emit_status_for_attempt(session_id, &attempt, terminal_status(Some(-9), None, None))
            .await;
        assert_eq!(
            store
                .get(session_id)
                .await
                .and_then(|record| record.outcome)
                .map(|outcome| outcome.reason),
            Some(LaunchSessionExitReason::LauncherStopped)
        );
        gated.publish_user_stop_reap(Ok(()));
        tokio::time::timeout(
            Duration::from_secs(2),
            store.lifecycle_transition.clone().lock_owned(),
        )
        .await
        .expect("detached kill settlement");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_process_reuse_resets_watchdog_and_attempt_state() {
        let store = Arc::new(SessionStore::new());
        let session_id = "reused-process-entry";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let stale_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("stale attempt");
        {
            let mut sessions = store.sessions.write().await;
            let entry = sessions.get_mut(session_id).expect("session entry");
            entry.stop_requested = true;
            entry.observed_failure = Some(ObservedFailureSignal {
                class: LaunchFailureClass::JvmUnsupportedOption,
                observed_at_ms: now_ms(),
            });
            entry.log_count = 7;
            entry.terminal_sequence = Some(11);
            entry.record.state = LaunchState::Exited;
            entry.record.boot_completed_at_ms = Some(now_ms());
            entry.record.boot_duration_ms = Some(10);
        }
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");

        let relaunched = store
            .start_process(test_record(session_id), command)
            .await
            .expect("relaunch reused session");
        let current_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("current attempt");

        assert_ne!(current_attempt.id, stale_attempt.id);
        assert_eq!(relaunched.state, LaunchState::Starting);
        assert_eq!(relaunched.boot_completed_at_ms, None);
        assert_eq!(relaunched.boot_duration_ms, None);
        {
            let sessions = store.sessions.read().await;
            let entry = sessions.get(session_id).expect("reused session entry");
            assert!(!entry.stop_requested);
            assert_eq!(entry.observed_failure, None);
            assert_eq!(entry.log_count, 0);
            assert_eq!(entry.terminal_sequence, None);
            assert_eq!(entry.record.state, LaunchState::Starting);
            assert_eq!(entry.record.boot_completed_at_ms, None);
            assert_eq!(entry.record.boot_duration_ms, None);
        }
        assert!(
            store
                .startup_watchdog_process_for_attempt(session_id, &current_attempt)
                .await
                .is_some()
        );
        store.terminate_all().await.expect("terminate relaunch");
    }

    #[tokio::test]
    async fn concurrent_and_repeated_shutdown_calls_are_idempotent() {
        let store = Arc::new(SessionStore::new());
        store
            .insert(test_record("shutdown-idempotent"))
            .await
            .expect("insert session");

        let (first, second) = tokio::join!(store.terminate_all(), store.terminate_all());
        first.expect("first shutdown succeeds");
        second.expect("concurrent shutdown succeeds");
        store
            .terminate_all()
            .await
            .expect("repeated shutdown succeeds");

        assert!(store.sessions.read().await.is_empty());
        assert!(store.active_processes.lock().await.is_empty());
        assert!(
            store
                .insert(test_record("shutdown-stays-latched"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_latches_before_the_owned_coordinator_is_scheduled() {
        let store = Arc::new(SessionStore::new());
        let mut shutdown = Box::pin(store.terminate_all());
        let mut completed = None;

        std::future::poll_fn(|context| {
            if let std::task::Poll::Ready(result) = shutdown.as_mut().poll(context) {
                completed = Some(result);
            }
            std::task::Poll::Ready(())
        })
        .await;

        assert!(store.shutdown_started.load(Ordering::Acquire));
        match completed {
            Some(result) => result.expect("shutdown succeeds"),
            None => shutdown.await.expect("shutdown succeeds"),
        }
    }

    #[tokio::test]
    async fn failed_spawn_preserves_the_session_attempt() {
        let store = Arc::new(SessionStore::new());
        let session_id = "failed-start-registration";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let original_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("original attempt");
        let command = Command::new("/definitely/missing/axial-java-runtime");

        assert!(
            store
                .start_process(test_record(session_id), command)
                .await
                .is_err()
        );

        let current_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("current attempt");
        assert!(attempt_scopes_match(&current_attempt, &original_attempt));
        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(stored.state, LaunchState::Queued);
        assert_eq!(stored.pid, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_post_spawn_registration_kills_only_the_unregistered_child() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-post-spawn-registration";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut old_command = Command::new("sh");
        old_command.arg("-c").arg("exec sleep 30");
        let old_child = attach_test_child(&store, session_id, old_command).await;
        let original_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("original attempt");
        let pid_path = test_pid_path("cancelled-registration");
        let _ = std::fs::remove_file(&pid_path);
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf '%s' $$ > \"$1\"; exec sleep 30")
            .arg("cancelled-registration")
            .arg(&pid_path);
        let sessions = store.sessions.read().await;
        let start_store = store.clone();
        let start = tokio::spawn(async move {
            start_store
                .start_process(test_record(session_id), command)
                .await
        });
        let spawned_pid = wait_for_pid_file(&pid_path).await;
        start.abort();
        assert!(start.await.expect_err("cancelled start").is_cancelled());
        drop(sessions);
        let _ = std::fs::remove_file(&pid_path);

        assert_process_exits(spawned_pid).await;
        assert!(process_is_live(old_child.pid));
        let current_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("current attempt");
        assert!(attempt_scopes_match(&current_attempt, &original_attempt));
        let mut cleanup = old_child
            .control
            .terminate(supervisor::ProcessTerminalCause::Replacement);
        cleanup.reaped().await.expect("cleanup old child");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_insert_waiting_for_store_does_not_signal_the_old_child() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-insert-replacement";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut old_command = Command::new("sh");
        old_command.arg("-c").arg("exec sleep 30");
        let old_child = attach_test_child(&store, session_id, old_command).await;
        let original_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("original attempt");
        let sessions = store.sessions.read().await;
        let insert_store = store.clone();
        let insert = tokio::spawn(async move {
            insert_store
                .insert(test_record(session_id))
                .await
                .expect("insert session");
        });
        tokio::task::yield_now().await;
        insert.abort();
        assert!(insert.await.expect_err("cancelled insert").is_cancelled());
        drop(sessions);

        assert!(process_is_live(old_child.pid));
        let current_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("current attempt");
        assert!(attempt_scopes_match(&current_attempt, &original_attempt));
        let mut cleanup = old_child
            .control
            .terminate(supervisor::ProcessTerminalCause::Replacement);
        cleanup.reaped().await.expect("cleanup old child");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_replacements_allocate_monotonic_attempts_and_stale_exit_is_ignored() {
        let store = Arc::new(SessionStore::new());
        let session_id = "concurrent-process-replacement";
        let mut first_command = Command::new("sh");
        first_command.arg("-c").arg("exec sleep 30");
        store
            .start_process(test_record(session_id), first_command)
            .await
            .expect("first process");
        let first_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("first attempt");

        let mut second_record = test_record(session_id);
        second_record.version_id = "second".to_string();
        let mut second_command = Command::new("sh");
        second_command.arg("-c").arg("exec sleep 30");
        let second_store = store.clone();
        let second = async move {
            second_store
                .start_process(second_record, second_command)
                .await
        };
        let mut third_record = test_record(session_id);
        third_record.version_id = "third".to_string();
        let mut third_command = Command::new("sh");
        third_command.arg("-c").arg("exec sleep 30");
        let third_store = store.clone();
        let third = async move { third_store.start_process(third_record, third_command).await };
        let (second, third) = tokio::join!(second, third);
        second.expect("second replacement");
        third.expect("third replacement");

        let current_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("current attempt");
        assert_eq!(current_attempt.id, first_attempt.id + 2);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            store.get(session_id).await.expect("stored record").state,
            LaunchState::Starting
        );
        store.terminate_all().await.expect("terminate all");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_launch_failure_settlement_cannot_claim_a_replacement_attempt() {
        let store = Arc::new(SessionStore::new());
        let session_id = "pending-launch-failure-replacement";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn replacement target");
        assert!(store.reject_next_process_start_kill(session_id).await);
        let pending = match store.terminate_for_launch_failure(session_id).await {
            LaunchFailureTermination::Pending(pending) => pending,
            _ => panic!("rejected termination must remain pending"),
        };

        let mut replacement = test_record(session_id);
        replacement.version_id = "replacement".to_string();
        store.insert(replacement).await.expect("insert session");
        let error_class =
            match tokio::time::timeout(Duration::from_secs(2), pending.wait_for_settlement())
                .await
                .expect("stale settlement deadline")
            {
                Ok(_) => panic!("stale attempt must not acquire a terminalization lease"),
                Err(error_class) => error_class,
            };

        assert_eq!(
            error_class,
            LaunchFailureTerminationErrorClass::StaleAttempt
        );
        let stored = store.get(session_id).await.expect("replacement session");
        assert_eq!(stored.version_id, "replacement");
        assert_eq!(stored.state, LaunchState::Queued);
        assert!(stored.failure.is_none());
        assert_eq!(store.retention_hold_count(session_id).await, Some(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_process_exit_preserves_observed_failure_class_without_raw_detail() {
        let store = Arc::new(SessionStore::new());
        let session_id = "class-only-observed-failure";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "printf '%s\\n' \"Unrecognized VM option '-XX:+UseZGC' /home/alice/.axial/secret\" >&2; sleep 0.2; exit 1",
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
        let status_json = serde_json::to_string(&status).expect("terminal status json");
        assert!(status_json.contains("execution_process_exited"));
        assert!(status_json.contains("execution_process_exit_code"));
        assert_public_session_payload_excludes_sensitive_content(&status_json);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exit_drains_the_final_output_failure_before_classification() {
        let store = Arc::new(SessionStore::new());
        let session_id = "drained-final-output-failure";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "i=0; while [ $i -lt 5000 ]; do printf '%s\\n' '[main/INFO]: loading modded game resources' >&2; i=$((i + 1)); done; printf '%s\\n' \"Unrecognized VM option '-XX:+UseZGC'\" >&2; exit 1",
        );

        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn output-heavy failing process");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let terminal = loop {
            let record = store.get(session_id).await.expect("session record");
            if classify::is_terminal_state(record.state) {
                break record;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "process did not reach terminal state"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        assert_eq!(terminal.state, LaunchState::Exited);
        assert_eq!(
            terminal.failure.map(|failure| failure.class),
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
    }

    #[tokio::test]
    async fn launch_boot_marker_records_completion_and_duration() {
        let store = Arc::new(SessionStore::new());
        let mut record = test_record("boot-marker-duration");
        record.state = LaunchState::Starting;
        record.process_started_at_ms = Some(now_ms().saturating_sub(4_200));
        store.insert(record).await.expect("insert session");

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
        let status_json = serde_json::to_string(&status).expect("boot status json");
        assert!(status_json.contains("execution_process_boot_evidence"));
        assert_public_session_payload_excludes_sensitive_content(&status_json);

        let stored = store
            .get("boot-marker-duration")
            .await
            .expect("stored record");
        assert_eq!(stored.state, LaunchState::Running);
        let stored_json = serde_json::to_string(&stored.stages).expect("stored stages json");
        assert!(stored_json.contains("execution_process_boot_evidence"));
        assert_public_session_payload_excludes_sensitive_content(&stored_json);
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
                Some("promoted" | "missing_process_handle" | "failed")
            ));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(priority.start_mode, "noop");
            assert_eq!(priority.promotion.as_deref(), Some("noop"));
        }
    }

    #[tokio::test]
    async fn log_preparation_and_priority_promotion_run_outside_the_store_write_lock() {
        let store = SessionStore::new();
        let session_id = "log-lock-discipline";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        record.process_started_at_ms = Some(now_ms().saturating_sub(1_000));
        store.insert(record).await.expect("insert session");
        {
            let mut sessions = store.sessions.write().await;
            sessions
                .get_mut(session_id)
                .expect("session entry")
                .observed_failure = Some(ObservedFailureSignal {
                class: LaunchFailureClass::ClasspathModuleConflict,
                observed_at_ms: now_ms(),
            });
        }
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let preparation_ran = AtomicBool::new(false);
        let promotion_ran = AtomicBool::new(false);

        emit_test_log_with_prepare(
            &store,
            session_id,
            r"C:\Users\Alice\AppData\stdout".to_string(),
            "[Render thread/INFO]: LWJGL Version: 3.3.3; Unrecognized VM option '-XX:+UseZGC'"
                .to_string(),
            |session_id, source, text, observed_at_ms| {
                let available = store
                    .sessions
                    .try_write()
                    .expect("log preparation must run outside the session write lock");
                drop(available);
                preparation_ran.store(true, Ordering::Relaxed);
                prepare_log_line(session_id, source, text, observed_at_ms)
            },
            |child| {
                let available = store
                    .sessions
                    .try_write()
                    .expect("priority promotion must run after releasing the session write lock");
                drop(available);
                assert!(child.is_none());
                promotion_ran.store(true, Ordering::Relaxed);
                Ok("test_promoted")
            },
        )
        .await;

        assert!(preparation_ran.load(Ordering::Relaxed));
        assert!(promotion_ran.load(Ordering::Relaxed));
        let LaunchEvent::Status(status) = receiver.recv().await.expect("running status") else {
            panic!("expected status before log event");
        };
        assert_eq!(status.state, "running");
        let LaunchEvent::Log(log) = receiver.recv().await.expect("log event") else {
            panic!("expected log event after status");
        };
        assert_eq!(log.source, "game");
        assert_eq!(log.text, crate::observability::PUBLIC_LOG_LINE_REDACTED);
        assert_eq!(
            store.observed_failure(session_id).await,
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        let stored = store.get(session_id).await.expect("stored record");
        assert!(stored.priority.and_then(|value| value.promotion).is_some());
        assert!(
            serde_json::to_string(&stored.stages)
                .expect("stage evidence json")
                .contains("execution_process_boot_evidence")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boot_stability_waits_for_priority_promotion_before_status_and_proof_visibility() {
        let store = Arc::new(SessionStore::new());
        let session_id = "boot-promotion-stability";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        record.process_started_at_ms = Some(now_ms().saturating_sub(1_000));
        store.insert(record).await.expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let effect_started = Arc::new(AtomicBool::new(false));
        let release_effect = Arc::new(std::sync::Barrier::new(2));

        let emit_store = store.clone();
        let emit_effect_started = effect_started.clone();
        let emit_release_effect = release_effect.clone();
        let emit = tokio::spawn(async move {
            emit_test_log(
                &emit_store,
                session_id,
                "stdout".to_string(),
                "[Render thread/INFO]: LWJGL Version: 3.3.3".to_string(),
                |_| {
                    emit_effect_started.store(true, Ordering::Release);
                    emit_release_effect.wait();
                    Ok("test_promoted")
                },
            )
            .await;
        });
        while !effect_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        let later_store = store.clone();
        let later_started = Arc::new(AtomicBool::new(false));
        let later_task_started = later_started.clone();
        let later_log = tokio::spawn(async move {
            later_task_started.store(true, Ordering::Release);
            later_store
                .emit_log(
                    session_id,
                    "stdout",
                    "[main/INFO]: Loading later modded game resources",
                )
                .await;
        });
        while !later_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;

        let pending_outcome = store.wait_for_startup(session_id, Duration::ZERO).await;
        let pending = store.get(session_id).await;
        let pending_stream_empty = receiver.try_recv().is_err();
        let later_log_pending = !later_log.is_finished();
        release_effect.wait();
        emit.await.expect("boot log task");
        later_log.await.expect("later log task");

        assert_eq!(pending_outcome, StartupOutcome::Stalled);
        let pending = pending.expect("pending boot record");
        assert_eq!(pending.state, LaunchState::Starting);
        assert_eq!(pending.boot_completed_at_ms, None);
        assert_eq!(pending.priority, None);
        assert!(pending_stream_empty);
        assert!(later_log_pending);
        assert_eq!(
            store.wait_for_startup(session_id, Duration::ZERO).await,
            StartupOutcome::Stable
        );
        let completed = store.get(session_id).await.expect("completed boot record");
        assert_eq!(completed.state, LaunchState::Running);
        assert!(completed.boot_completed_at_ms.is_some());
        assert_eq!(
            completed
                .priority
                .and_then(|priority| priority.promotion)
                .as_deref(),
            Some("test_promoted")
        );
        let LaunchEvent::Status(status) = receiver.recv().await.expect("running status") else {
            panic!("expected running status before logs");
        };
        assert_eq!(status.state, "running");
        let LaunchEvent::Log(boot_log) = receiver.recv().await.expect("boot log") else {
            panic!("expected boot log after running status");
        };
        assert!(boot_log.text.contains("LWJGL Version"));
        let LaunchEvent::Log(later_log) = receiver.recv().await.expect("later log") else {
            panic!("expected later log after boot log");
        };
        assert!(later_log.text.contains("later modded game resources"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_boot_effect_leaves_no_claim_or_partial_log_state() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-boot-effect";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        set_failure_observed_at(&store, session_id, now_ms()).await;
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let effect_started = Arc::new(std::sync::Barrier::new(2));
        let release_effect = Arc::new(std::sync::Barrier::new(2));
        let effect_finished = Arc::new(AtomicBool::new(false));

        let emit_store = store.clone();
        let emit_started = effect_started.clone();
        let emit_release = release_effect.clone();
        let emit_finished = effect_finished.clone();
        let emit = tokio::spawn(async move {
            emit_test_log(
                &emit_store,
                session_id,
                "stdout".to_string(),
                "[Render thread/INFO]: LWJGL Version: 3.3.3".to_string(),
                |_| {
                    emit_started.wait();
                    emit_release.wait();
                    emit_finished.store(true, Ordering::Release);
                    Ok("test_promoted")
                },
            )
            .await;
        });
        effect_started.wait();
        let sessions = store.sessions.write().await;
        release_effect.wait();
        while !effect_finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        emit.abort();
        assert!(emit.await.expect_err("cancelled boot emit").is_cancelled());
        drop(sessions);

        let sessions = store.sessions.read().await;
        let entry = sessions.get(session_id).expect("session entry");
        assert_eq!(entry.record.state, LaunchState::Starting);
        assert_eq!(entry.record.boot_completed_at_ms, None);
        assert_eq!(entry.record.priority, None);
        assert_eq!(entry.log_count, 0);
        assert_eq!(
            entry.observed_failure.map(|signal| signal.class),
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        drop(sessions);
        assert!(receiver.try_recv().is_err());
        assert!(attempt.log_transition.try_lock().is_ok());
    }

    #[tokio::test]
    async fn stale_attempt_output_and_exit_do_not_update_a_reused_session() {
        let store = SessionStore::new();
        let session_id = "stale-attempt-output";
        let stale_attempt = replace_session(&store, session_id).await;
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        store
            .emit_log_for_attempt(
                session_id,
                &stale_attempt,
                "stderr",
                "Unrecognized VM option '-XX:+UseZGC'".to_string(),
                now_ms(),
            )
            .await;
        store
            .emit_status_for_attempt(
                session_id,
                &stale_attempt,
                terminal_status(Some(1), Some("unknown"), None),
            )
            .await;

        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(stored.state, LaunchState::Starting);
        assert_eq!(stored.boot_completed_at_ms, None);
        assert_eq!(stored.exit_code, None);
        assert_eq!(store.observed_failure(session_id).await, None);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_attempt_watchdog_does_not_overwrite_a_retry() {
        let store = Arc::new(SessionStore::new());
        let session_id = "stale-attempt-watchdog";
        let stale_attempt = replace_session(&store, session_id).await;
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let (trigger, trigger_rx) = tokio::sync::oneshot::channel();

        let watchdog = supervisor::spawn_startup_watchdog_with_trigger(
            store.clone(),
            session_id.to_string(),
            stale_attempt,
            async move {
                let _ = trigger_rx.await;
            },
        );
        trigger.send(()).expect("trigger stale watchdog");
        watchdog.await.expect("stale watchdog task");

        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(stored.state, LaunchState::Starting);
        assert_eq!(stored.failure, None);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn late_monitoring_status_cannot_regress_a_running_attempt() {
        let store = SessionStore::new();
        let session_id = "late-monitoring-status";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let mut running = terminal_status(None, None, None);
        running.state = "running".to_string();
        store
            .emit_status_for_attempt(session_id, &attempt, running)
            .await;
        assert_eq!(recv_status(&mut receiver).await.state, "running");

        let mut late_monitoring = terminal_status(None, None, None);
        late_monitoring.state = "monitoring".to_string();
        store
            .emit_status_for_attempt(session_id, &attempt, late_monitoring)
            .await;

        assert_eq!(
            store.get(session_id).await.expect("stored record").state,
            LaunchState::Running
        );
        assert!(receiver.try_recv().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn late_startup_statuses_cannot_resurrect_a_fast_exit() {
        let store = Arc::new(SessionStore::new());
        let session_id = "late-status-after-fast-exit";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 9");
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("fast-exit process");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let state = store.get(session_id).await.expect("stored record").state;
            if classify::is_terminal_state(state) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fast-exit process did not reach terminal state"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        for state in ["monitoring", "running"] {
            let mut late = terminal_status(None, None, None);
            late.state = state.to_string();
            store
                .emit_status_for_attempt(session_id, &attempt, late)
                .await;
        }

        assert_eq!(
            store.get(session_id).await.expect("stored record").state,
            LaunchState::Exited
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn nonstartup_boot_marker_is_logged_without_claiming_boot() {
        let store = SessionStore::new();
        let session_id = "nonstartup-boot-marker";
        let mut record = test_record(session_id);
        record.state = LaunchState::Running;
        store.insert(record).await.expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        store
            .emit_log(
                session_id,
                "stdout",
                "[Render thread/INFO]: LWJGL Version: 3.3.3",
            )
            .await;

        let LaunchEvent::Log(log) = receiver.recv().await.expect("boot-looking log") else {
            panic!("expected log without status");
        };
        assert!(log.text.contains("LWJGL Version"));
        assert!(receiver.try_recv().is_err());
        let entry = store.sessions.read().await;
        let entry = entry.get(session_id).expect("session entry");
        assert_eq!(entry.record.boot_completed_at_ms, None);
    }

    #[tokio::test]
    async fn stop_adjacent_boot_marker_completes_with_skipped_proof_before_log() {
        let store = SessionStore::new();
        let session_id = "stop-adjacent-boot-marker";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        mark_stop_requested(&store, session_id).await;
        let effect_ran = AtomicBool::new(false);

        let stored = emit_test_boot(&store, session_id, "skipped_stop_requested", |_| {
            effect_ran.store(true, Ordering::Relaxed);
            Ok("test_promoted")
        })
        .await;

        assert!(!effect_ran.load(Ordering::Relaxed));
        assert_eq!(stored.state, LaunchState::Running);
    }

    #[tokio::test]
    async fn stop_racing_after_promotion_preserves_the_performed_effect_proof() {
        let store = SessionStore::new();
        let session_id = "stop-after-boot-promotion";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");

        emit_test_boot(&store, session_id, "test_promoted", |child| {
            assert!(child.is_none());
            store
                .sessions
                .try_write()
                .expect("promotion must run outside the session write lock")
                .get_mut(session_id)
                .expect("session entry")
                .stop_requested = true;
            Ok("test_promoted")
        })
        .await;
    }

    #[tokio::test]
    async fn pid_without_child_skips_real_boot_promotion_and_keeps_the_marker() {
        let store = SessionStore::new();
        let session_id = "boot-marker-missing-child";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        record.pid = Some(41);
        store.insert(record).await.expect("insert session");
        let effect_ran = AtomicBool::new(false);

        emit_test_boot(&store, session_id, "skipped_missing_process_handle", |_| {
            effect_ran.store(true, Ordering::Relaxed);
            Ok("test_promoted")
        })
        .await;

        assert!(!effect_ran.load(Ordering::Relaxed));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn boot_marker_after_physical_exit_completes_with_skipped_proof() {
        let store = SessionStore::new();
        let session_id = "boot-marker-after-physical-exit";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        let effect_ran = AtomicBool::new(false);

        emit_test_boot_reply(
            &store,
            session_id,
            "skipped_process_already_exited",
            supervisor::ProcessPriorityReply::ExitedBefore,
        )
        .await;

        assert!(!effect_ran.load(Ordering::Relaxed));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boot_marker_process_exit_during_promotion_completes_with_skipped_proof() {
        let store = Arc::new(SessionStore::new());
        let session_id = "boot-marker-exit-during-promotion";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        let effect_ran = AtomicBool::new(false);

        emit_test_boot_reply(
            &store,
            session_id,
            "skipped_process_exited_during_promotion",
            supervisor::ProcessPriorityReply::ExitedAfter,
        )
        .await;

        assert!(!effect_ran.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn failure_freshness_uses_the_exit_observation_boundary() {
        let store = SessionStore::new();
        let session_id = "failure-freshness-boundary";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let exit_observed_at_ms = now_ms();
        set_failure_observed_at(
            &store,
            session_id,
            exit_observed_at_ms - FAILURE_SIGNAL_VALID_FOR_EXIT_MS,
        )
        .await;
        assert_eq!(
            store
                .process_exit_context(session_id, &attempt, exit_observed_at_ms)
                .await
                .and_then(|context| context.observed_failure),
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        set_failure_observed_at(
            &store,
            session_id,
            exit_observed_at_ms - FAILURE_SIGNAL_VALID_FOR_EXIT_MS - 1,
        )
        .await;
        assert_eq!(
            store
                .process_exit_context(session_id, &attempt, exit_observed_at_ms)
                .await
                .and_then(|context| context.observed_failure),
            None
        );
    }

    #[tokio::test]
    async fn launch_log_events_preserve_normal_minecraft_markers() {
        let store = SessionStore::new();
        let session_id = "normal-log-marker";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        store
            .emit_log(
                session_id,
                "stdout",
                "[Render thread/INFO]: Reloading ResourceManager: vanilla",
            )
            .await;

        let emitted = receiver.recv().await.expect("log event");
        let LaunchEvent::Log(log) = emitted else {
            panic!("expected log event");
        };
        assert_eq!(log.source, "stdout");
        assert_eq!(
            log.text,
            "[Render thread/INFO]: Reloading ResourceManager: vanilla"
        );
    }

    #[tokio::test]
    async fn launch_log_events_redact_sensitive_public_lines() {
        let store = SessionStore::new();
        let session_id = "sensitive-log-line";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");

        store
            .emit_log(
                session_id,
                r"C:\Users\Alice\AppData\stdout",
                "failed for /home/alice/.minecraft java.exe --accessToken raw-secret-token -Xmx8192M -Dtoken=raw provider_payload=provider-secret account_id=account-secret username=SecretPlayer",
            )
            .await;

        let emitted = receiver.recv().await.expect("log event");
        let LaunchEvent::Log(log) = emitted else {
            panic!("expected log event");
        };
        assert_eq!(log.source, "game");
        assert_eq!(log.text, crate::observability::PUBLIC_LOG_LINE_REDACTED);
        let log_json = serde_json::to_string(&log).expect("log json");
        assert_public_session_payload_excludes_sensitive_content(&log_json);
    }

    #[tokio::test]
    async fn launch_external_close_after_boot_is_classified_cleanly() {
        let store = SessionStore::new();
        let session_id = "external-close-after-boot";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        record.process_started_at_ms = Some(now_ms().saturating_sub(1_000));
        store.insert(record).await.expect("insert session");

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

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_stop_request_records_execution_stop_intent_evidence() {
        let store = Arc::new(SessionStore::new());
        let session_id = "stop-intent-evidence";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 5");

        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn sleep process");
        store.kill(session_id).await.expect("kill process");

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
            status.outcome.as_ref().expect("stop outcome").reason,
            LaunchSessionExitReason::LauncherStopped
        );
        let status_json = serde_json::to_string(&status).expect("status json");
        assert!(status_json.contains("execution_process_stop_requested"));
        assert!(status_json.contains("execution_process_exited"));
        assert_public_session_payload_excludes_sensitive_content(&status_json);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_kill_requests_join_the_same_owner_reap() {
        let store = Arc::new(SessionStore::new());
        let session_id = "concurrent-owner-kill";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        let launched = store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn kill target");
        let pid = launched.pid.expect("kill target pid");

        let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(store.kill(session_id), store.kill(session_id))
        })
        .await
        .expect("concurrent kill deadline");

        first.expect("first kill");
        second.expect("second kill");
        assert!(!process_is_live(pid));
        let stored = store.get(session_id).await.expect("stored record");
        assert!(
            stored
                .stages
                .iter()
                .flat_map(|stage| &stage.evidence)
                .any(|evidence| evidence.id.contains("process_stop_requested"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn user_stop_lease_blocks_replacement_until_proof_scope_is_released() {
        let store = Arc::new(SessionStore::new());
        let session_id = "user-stop-blocks-replacement";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn stop target");
        let original_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("original attempt");

        let stop = store
            .begin_user_stop(session_id)
            .await
            .expect("begin user stop");
        assert!(store.lifecycle_transition.try_lock().is_err());
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("stopped session")
                .retention_holds,
            2
        );

        let (started, started_rx) = tokio::sync::oneshot::channel();
        let replacement_store = store.clone();
        let replacement = tokio::spawn(async move {
            let _ = started.send(());
            replacement_store
                .insert(test_record(session_id))
                .await
                .expect("insert session");
        });
        started_rx.await.expect("replacement started");
        tokio::task::yield_now().await;
        assert!(!replacement.is_finished());

        stop.release().await;
        tokio::time::timeout(Duration::from_secs(2), replacement)
            .await
            .expect("replacement deadline")
            .expect("replacement task");
        let replacement_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("replacement attempt");
        assert_ne!(replacement_attempt.id, original_attempt.id);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_user_stop_lease_releases_retention_and_lifecycle_guard() {
        let store = Arc::new(SessionStore::new());
        let session_id = "dropped-user-stop-lease";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn stop target");

        let stop = store
            .begin_user_stop(session_id)
            .await
            .expect("begin user stop");
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("stopped session")
                .retention_holds,
            2
        );
        drop(stop);

        let lifecycle = tokio::time::timeout(
            Duration::from_secs(2),
            store.lifecycle_transition.clone().lock_owned(),
        )
        .await
        .expect("drop cleanup deadline");
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("stopped session")
                .retention_holds,
            1
        );
        drop(lifecycle);
    }

    #[tokio::test]
    async fn failed_user_stop_rolls_back_retention_and_lifecycle_guard() {
        let store = Arc::new(SessionStore::new());
        let session_id = "failed-user-stop-lease";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        store
            .sessions
            .write()
            .await
            .get_mut(session_id)
            .expect("stop session")
            .process = Some(supervisor::rejected_process_control_handle());
        let before = store
            .get(session_id)
            .await
            .expect("record before rejected stop");

        let error = match store.begin_user_stop(session_id).await {
            Ok(_) => panic!("rejected owner must fail stop"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            SessionStopError::Process(error)
                if error.kind() == std::io::ErrorKind::BrokenPipe
        ));
        let sessions = store.sessions.read().await;
        let entry = sessions.get(session_id).expect("stop session");
        assert_eq!(entry.retention_holds, 1);
        assert!(!entry.stop_requested);
        assert_eq!(entry.record.stages, before.stages);
        drop(sessions);
        assert!(store.lifecycle_transition.try_lock().is_ok());
    }

    #[tokio::test]
    async fn unavailable_launch_failure_settlement_keeps_the_session_active_and_retained() {
        let store = Arc::new(SessionStore::new());
        let session_id = "unavailable-launch-failure-settlement";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        {
            let mut sessions = store.sessions.write().await;
            let entry = sessions.get_mut(session_id).expect("session entry");
            entry.record.pid = Some(42);
            entry.process = Some(supervisor::rejected_process_control_handle());
        }

        let error_class = match store.terminate_for_launch_failure(session_id).await {
            LaunchFailureTermination::Unconfirmed(error_class) => error_class,
            _ => panic!("unavailable owner must remain unconfirmed"),
        };

        assert_eq!(
            error_class,
            LaunchFailureTerminationErrorClass::OwnerUnavailable
        );
        let stored = store.get(session_id).await.expect("retained session");
        assert!(!matches!(
            stored.state,
            LaunchState::Failed | LaunchState::Exited
        ));
        assert!(store.has_active_instance(&stored.instance_id).await);
        assert_eq!(store.retention_hold_count(session_id).await, Some(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_begin_user_stop_detaches_retention_cleanup() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-begin-user-stop";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let gated = attach_reap_gated_test_child(&store, session_id).await;
        let stop_store = store.clone();
        let stop = tokio::spawn(async move { stop_store.begin_user_stop(session_id).await });

        gated.reap_reached.await.expect("owner reached reap gate");
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("stop session")
                .retention_holds,
            2
        );
        assert!(store.lifecycle_transition.try_lock().is_err());
        stop.abort();
        match stop.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("stop task was not cancelled"),
        }
        let _ = gated.release_reap.send(());

        let lifecycle = tokio::time::timeout(
            Duration::from_secs(2),
            store.lifecycle_transition.clone().lock_owned(),
        )
        .await
        .expect("cancel cleanup deadline");
        assert_eq!(
            store
                .sessions
                .read()
                .await
                .get(session_id)
                .expect("stop session")
                .retention_holds,
            1
        );
        drop(lifecycle);
        let mut completed = gated.control.completion_receiver();
        if !*completed.borrow() {
            tokio::time::timeout(Duration::from_secs(2), completed.changed())
                .await
                .expect("owner completion deadline")
                .expect("owner completion");
        }
    }

    #[tokio::test]
    async fn cancelled_begin_user_stop_pending_acceptance_releases_retention_after_acceptance() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-stop-acceptance";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let mut gated = supervisor::gated_termination_control();
        store
            .sessions
            .write()
            .await
            .get_mut(session_id)
            .expect("session")
            .process = Some(gated.handle.clone());
        let stop_store = store.clone();
        let stop = tokio::spawn(async move { stop_store.begin_user_stop(session_id).await });
        gated.capture_user_stop_request().await;
        assert_eq!(store.retention_hold_count(session_id).await, Some(2));

        stop.abort();
        match stop.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("stop task was not cancelled"),
        }
        gated.accept_user_stop();
        wait_for_user_stop_intent(&store, session_id).await;
        assert_eq!(user_stop_evidence_count(&store, session_id).await, 1);
        store
            .emit_status_for_attempt(session_id, &attempt, terminal_status(Some(-9), None, None))
            .await;
        gated.publish_user_stop_reap(Ok(()));
        let guard = tokio::time::timeout(
            Duration::from_secs(2),
            store.lifecycle_transition.clone().lock_owned(),
        )
        .await
        .expect("detached stop settlement");
        assert_eq!(store.retention_hold_count(session_id).await, Some(1));
        assert_eq!(
            store
                .get(session_id)
                .await
                .and_then(|record| record.outcome)
                .map(|outcome| outcome.reason),
            Some(LaunchSessionExitReason::LauncherStopped)
        );
        drop(guard);
    }

    #[tokio::test]
    async fn cancelled_user_stop_reap_error_keeps_blocked_retention_cleanup_owned() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-stop-reap-error-cleanup";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let (attempt, prior_terminal_sequence) = {
            let mut sessions = store.sessions.write().await;
            let entry = sessions.get_mut(session_id).expect("session");
            entry.retention_holds = entry
                .retention_holds
                .checked_add(1)
                .expect("retention hold count");
            let prior_terminal_sequence = entry.terminal_sequence;
            entry.terminal_sequence = None;
            (entry.attempt.clone(), prior_terminal_sequence)
        };
        let lifecycle_guard = store.lifecycle_transition.clone().lock_owned().await;
        let mut gated = supervisor::gated_termination_control();
        let request = gated
            .handle
            .terminate(supervisor::ProcessTerminalCause::UserStop);
        gated.capture_user_stop_request().await;
        let mut stop = PendingUserStop {
            store: store.clone(),
            session_id: session_id.to_string(),
            attempt,
            request: Some(request),
            lifecycle_guard: Some(lifecycle_guard),
            prior_terminal_sequence: Some(prior_terminal_sequence),
            acceptance_rejected: false,
        };
        gated.accept_user_stop();
        let acceptance = stop.accepted().await.expect("accepted user stop");
        stop.publish_intent(acceptance)
            .await
            .expect("current user-stop attempt");
        gated.publish_user_stop_reap(Err(std::io::Error::other("injected reap failure")));
        stop.reaped().await.expect_err("errored user-stop reap");

        let sessions = store.sessions.write().await;
        let (cleanup_started, cleanup_started_rx) = tokio::sync::oneshot::channel();
        let cleanup = tokio::spawn(async move {
            let _ = cleanup_started.send(());
            stop.release_retention().await;
            stop.disarm_request();
            stop.release_lifecycle_guard();
        });
        cleanup_started_rx.await.expect("cleanup started");
        cleanup.abort();
        assert!(cleanup.await.expect_err("cancelled cleanup").is_cancelled());
        drop(sessions);

        let lifecycle = tokio::time::timeout(
            Duration::from_secs(2),
            store.lifecycle_transition.clone().lock_owned(),
        )
        .await
        .expect("detached cleanup deadline");
        assert_eq!(store.retention_hold_count(session_id).await, Some(1));
        assert_eq!(user_stop_evidence_count(&store, session_id).await, 1);
        drop(lifecycle);
    }

    #[tokio::test]
    async fn cancelled_user_stop_rejection_keeps_blocked_rollback_owned() {
        let store = Arc::new(SessionStore::new());
        let session_id = "cancelled-stop-rejection-cleanup";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let prior_terminal_sequence = Some(17);
        let attempt = {
            let mut sessions = store.sessions.write().await;
            let entry = sessions.get_mut(session_id).expect("session");
            entry.retention_holds = entry
                .retention_holds
                .checked_add(1)
                .expect("retention hold count");
            entry.terminal_sequence = None;
            entry.attempt.clone()
        };
        let lifecycle_guard = store.lifecycle_transition.clone().lock_owned().await;
        let mut gated = supervisor::gated_termination_control();
        let request = gated
            .handle
            .terminate(supervisor::ProcessTerminalCause::UserStop);
        gated.capture_user_stop_request().await;
        let mut stop = PendingUserStop {
            store: store.clone(),
            session_id: session_id.to_string(),
            attempt,
            request: Some(request),
            lifecycle_guard: Some(lifecycle_guard),
            prior_terminal_sequence: Some(prior_terminal_sequence),
            acceptance_rejected: false,
        };
        gated.reject_user_stop(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected user-stop rejection",
        ));
        stop.accepted().await.expect_err("rejected user stop");

        let sessions = store.sessions.write().await;
        let (rollback_started, rollback_started_rx) = tokio::sync::oneshot::channel();
        let rollback = tokio::spawn(async move {
            let _ = rollback_started.send(());
            stop.rollback_rejection().await;
        });
        rollback_started_rx.await.expect("rollback started");
        rollback.abort();
        assert!(
            rollback
                .await
                .expect_err("cancelled rollback")
                .is_cancelled()
        );
        drop(sessions);

        let lifecycle = tokio::time::timeout(
            Duration::from_secs(2),
            store.lifecycle_transition.clone().lock_owned(),
        )
        .await
        .expect("detached rollback deadline");
        let sessions = store.sessions.read().await;
        let entry = sessions.get(session_id).expect("session");
        assert_eq!(entry.retention_holds, 1);
        assert_eq!(entry.terminal_sequence, prior_terminal_sequence);
        assert!(!entry.stop_requested);
        drop(sessions);
        drop(lifecycle);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_acknowledges_reap_before_inherited_output_pipe_drain_completes() {
        let store = Arc::new(SessionStore::new());
        let session_id = "reap-before-inherited-pipe-drain";
        let descendant_pid_path = test_pid_path("inherited-output-pipe");
        let _ = std::fs::remove_file(&descendant_pid_path);
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30 & descendant=$!; printf '%s' \"$descendant\" > \"$1\"; exec sleep 30")
            .arg("inherited-output-pipe")
            .arg(&descendant_pid_path);
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn inherited-pipe target");
        let descendant_pid = wait_for_pid_file(&descendant_pid_path).await;
        let control = store
            .sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.process.clone())
            .expect("process control");

        tokio::time::timeout(Duration::from_millis(250), store.kill(session_id))
            .await
            .expect("reap acknowledgment before drain timeout")
            .expect("kill target");
        let mut completed = control.completion_receiver();
        assert!(!*completed.borrow());
        tokio::time::timeout(Duration::from_secs(2), completed.changed())
            .await
            .expect("bounded output drain completion")
            .expect("owner completion");

        let _ = std::process::Command::new("kill")
            .args(["-9", &descendant_pid.to_string()])
            .status();
        let _ = std::fs::remove_file(&descendant_pid_path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_all_settles_every_current_owner_before_clearing_sessions() {
        let store = Arc::new(SessionStore::new());
        let mut pids = Vec::new();
        let mut receivers = Vec::new();
        for session_id in ["shutdown-owner-a", "shutdown-owner-b"] {
            let mut command = Command::new("sh");
            command.arg("-c").arg("exec sleep 30");
            let launched = store
                .start_process(test_record(session_id), command)
                .await
                .expect("spawn shutdown target");
            pids.push(launched.pid.expect("shutdown target pid"));
            let mut receiver = store.subscribe(session_id).await.expect("subscribe");
            while receiver.try_recv().is_ok() {}
            receivers.push(receiver);
        }

        tokio::time::timeout(Duration::from_secs(2), store.terminate_all())
            .await
            .expect("shutdown reap deadline")
            .expect("shutdown owners");

        assert!(pids.into_iter().all(|pid| !process_is_live(pid)));
        assert!(store.active_processes.lock().await.is_empty());
        assert!(store.sessions.read().await.is_empty());
        for receiver in &mut receivers {
            assert!(receiver.try_recv().is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_shutdown_waiting_for_admitted_launch_keeps_detached_coordinator() {
        let store = Arc::new(SessionStore::new());
        let session_id = "shutdown-cancelled-before-gate";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let pid_path = test_pid_path("shutdown-cancelled-before-gate");
        let _ = std::fs::remove_file(&pid_path);
        let sessions = store.sessions.read().await;
        let start_store = store.clone();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf '%s' $$ > \"$1\"; exec sleep 30")
            .arg("shutdown-cancelled-before-gate")
            .arg(&pid_path);
        let start = tokio::spawn(async move {
            start_store
                .start_process(test_record(session_id), command)
                .await
        });
        let pid = wait_for_pid_file(&pid_path).await;

        let shutdown_store = store.clone();
        let shutdown = tokio::spawn(async move { shutdown_store.terminate_all().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.shutdown_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown latch deadline");
        shutdown.abort();
        assert!(
            shutdown
                .await
                .expect_err("cancelled shutdown")
                .is_cancelled()
        );
        assert!(store.shutdown_started.load(Ordering::Acquire));

        drop(sessions);
        let launched = tokio::time::timeout(Duration::from_secs(2), start)
            .await
            .expect("admitted launch registration deadline")
            .expect("admitted launch task")
            .expect("admitted launch registers");
        assert_eq!(launched.pid, Some(pid));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.active_processes.lock().await.is_empty()
                    && store.sessions.read().await.is_empty()
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("original shutdown coordinator completion");
        assert!(!process_is_live(pid));
        assert!(store.active_processes.lock().await.is_empty());
        assert!(store.sessions.read().await.is_empty());
        store
            .terminate_all()
            .await
            .expect("later shutdown observes idempotent completion");
        let _ = std::fs::remove_file(pid_path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_latch_rejects_queued_insert_and_relaunch_and_is_idempotent() {
        let store = Arc::new(SessionStore::new());
        let session_id = "shutdown-admission-latch";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let gated = attach_reap_gated_test_child(&store, session_id).await;
        let shutdown_store = store.clone();
        let shutdown = tokio::spawn(async move { shutdown_store.terminate_all().await });
        gated
            .reap_reached
            .await
            .expect("shutdown reaches process reap gate");

        let insert_store = store.clone();
        let insert = tokio::spawn(async move {
            insert_store
                .insert(test_record("insert-after-shutdown"))
                .await
        });
        let start_store = store.clone();
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        let start = tokio::spawn(async move {
            start_store
                .start_process(test_record(session_id), command)
                .await
        });
        tokio::task::yield_now().await;
        assert!(!insert.is_finished());
        assert!(!start.is_finished());

        gated.release_reap.send(()).expect("release process reap");
        shutdown
            .await
            .expect("shutdown task")
            .expect("shutdown succeeds");
        insert
            .await
            .expect("insert task")
            .expect_err("insert is rejected after shutdown begins");
        let error = start
            .await
            .expect("relaunch task")
            .expect_err("relaunch is rejected after shutdown begins");
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
        assert_eq!(error.to_string(), "launch session store is shutting down");
        assert!(store.sessions.read().await.is_empty());
        assert!(store.active_processes.lock().await.is_empty());
        store
            .terminate_all()
            .await
            .expect("repeated shutdown is idempotent");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_all_waits_for_output_settlement_and_exact_owner_removal() {
        let store = Arc::new(SessionStore::new());
        let session_id = "shutdown-output-settlement";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("process attempt");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("shutdown child");
        let (control, owner) = supervisor::prepare_process_owner(child);
        let release_output = Arc::new(Notify::new());
        let output_release = release_output.clone();
        {
            let mut active = store.active_processes.lock().await;
            let mut sessions = store.sessions.write().await;
            sessions.get_mut(session_id).expect("session").process = Some(control.clone());
            active.insert(attempt.id, control.clone());
            owner.spawn(
                store.clone(),
                session_id.to_string(),
                attempt.clone(),
                supervisor::output_pump_tasks_with_processor(tokio::spawn(async move {
                    output_release.notified().await;
                })),
            );
        }
        let shutdown_store = store.clone();
        let shutdown = tokio::spawn(async move { shutdown_store.terminate_all().await });
        control.wait_until_reaped().await;

        assert!(!shutdown.is_finished());
        assert!(
            store
                .active_processes
                .lock()
                .await
                .contains_key(&attempt.id)
        );
        assert!(store.get(session_id).await.is_some());

        release_output.notify_one();
        shutdown
            .await
            .expect("shutdown task")
            .expect("shutdown waits for owner completion");
        assert!(
            !store
                .active_processes
                .lock()
                .await
                .contains_key(&attempt.id)
        );
        assert!(store.sessions.read().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_watchdog_kills_outputting_process_without_boot_marker() {
        let store = Arc::new(SessionStore::new());
        let session_id = "watchdog-no-boot-marker";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf '%s\\n' 'ordinary startup output'; exec sleep 30");

        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn stalled process");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("watchdog attempt");
        let (trigger, trigger_rx) = tokio::sync::oneshot::channel();
        let watchdog = supervisor::spawn_startup_watchdog_with_trigger(
            store.clone(),
            session_id.to_string(),
            attempt,
            async move {
                let _ = trigger_rx.await;
            },
        );

        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .expect("ordinary output deadline")
                .expect("ordinary output broadcast");
            if let LaunchEvent::Log(log) = event
                && log.text.contains("ordinary startup output")
            {
                break;
            }
        }
        trigger.send(()).expect("trigger watchdog");

        let terminal_status = loop {
            let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .expect("watchdog event")
                .expect("watchdog broadcast");
            if let LaunchEvent::Status(status) = event
                && status.state == "exited"
            {
                break Some(status);
            }
        };

        let status = terminal_status.expect("watchdog terminal status");
        assert_eq!(
            status.failure_class.as_deref(),
            Some(LaunchFailureClass::StartupStalled.as_str())
        );
        assert_eq!(
            status.outcome.as_ref().expect("watchdog outcome").reason,
            LaunchSessionExitReason::WatchdogKilled
        );
        let status_json = serde_json::to_string(&status).expect("status json");
        assert!(status_json.contains("execution_process_killed"));
        assert!(status_json.contains("execution_process_watchdog_action"));
        assert_public_session_payload_excludes_sensitive_content(&status_json);
        watchdog.await.expect("watchdog task");
    }

    #[tokio::test]
    async fn stale_failure_signal_is_ignored_for_later_clean_exit() {
        let store = SessionStore::new();
        let session_id = "stale-failure-clean-exit";
        store
            .insert(terminal_record(session_id, LaunchState::Running, true))
            .await
            .expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("process attempt");

        {
            let mut sessions = store.sessions.write().await;
            let entry = sessions.get_mut(session_id).expect("session entry");
            entry.observed_failure = Some(ObservedFailureSignal {
                class: LaunchFailureClass::JvmUnsupportedOption,
                observed_at_ms: now_ms().saturating_sub(FAILURE_SIGNAL_VALID_FOR_EXIT_MS + 1),
            });
        }

        assert_eq!(
            store.observed_failure(session_id).await,
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        let exit_failure = store
            .process_exit_context(session_id, &attempt, now_ms())
            .await
            .and_then(|context| context.observed_failure);
        assert_eq!(exit_failure, None);

        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        store
            .emit_status(
                session_id,
                LaunchStatusEvent {
                    state: "exited".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: Some(0),
                    failure_class: exit_failure
                        .map(|failure_class| failure_class.as_str().to_string()),
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

        let status = recv_status(&mut receiver).await;
        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(stored.failure, None);
        assert_eq!(
            stored.outcome.expect("stored outcome").reason,
            LaunchSessionExitReason::ExternalUserClosed
        );
        assert_eq!(status.notice, None);
        let status_json = serde_json::to_string(&status).expect("status json");
        assert!(!status_json.to_ascii_lowercase().contains("crash"));
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
            Some(axial_launcher::LaunchNoticeTone::Error)
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
        store
            .insert(launcher_stop_record)
            .await
            .expect("insert session");
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
            Some(axial_launcher::LaunchNoticeTone::Error)
        );
    }

    #[tokio::test]
    async fn launch_running_status_without_boot_marker_does_not_record_boot_duration() {
        let store = SessionStore::new();
        let mut record = test_record("timeout-running");
        record.state = LaunchState::Monitoring;
        record.process_started_at_ms = Some(now_ms().saturating_sub(5_000));
        store.insert(record).await.expect("insert session");

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
        store.insert(active).await.expect("insert session");

        let mut queued_without_command = test_record("queued-without-command");
        queued_without_command.command = Vec::new();
        store
            .insert(queued_without_command)
            .await
            .expect("insert session");

        let mut exited = test_record("exited-memory");
        exited.state = LaunchState::Exited;
        exited.command = vec!["java".to_string(), "-Xmx8192M".to_string()];
        store.insert(exited).await.expect("insert session");

        assert_eq!(store.active_memory_allocation_mb().await, 2048);
    }

    #[tokio::test]
    async fn launch_active_session_count_excludes_terminal_sessions() {
        let store = SessionStore::new();
        store
            .insert(test_record("queued-count"))
            .await
            .expect("insert session");

        let mut starting = test_record("starting-count");
        starting.state = LaunchState::Starting;
        store.insert(starting).await.expect("insert session");

        let mut exited = test_record("exited-count");
        exited.state = LaunchState::Exited;
        store.insert(exited).await.expect("insert session");

        assert_eq!(store.active_session_count().await, 2);
    }

    #[tokio::test]
    async fn launch_active_session_id_lookup_ignores_missing_and_terminal_sessions() {
        let store = SessionStore::new();
        let mut failed = test_record("failed-session");
        failed.state = LaunchState::Failed;
        store.insert(failed).await.expect("insert session");

        let mut exited = test_record("exited-session");
        exited.state = LaunchState::Exited;
        store.insert(exited).await.expect("insert session");

        assert!(
            !store
                .has_any_active_session_id(["missing-session", "failed-session", "exited-session"])
                .await
        );
    }

    #[tokio::test]
    async fn launch_active_session_id_lookup_detects_non_terminal_sessions() {
        let store = SessionStore::new();
        store
            .insert(test_record("queued-session"))
            .await
            .expect("insert session");

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
        store.insert(record).await.expect("insert session");
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
            LaunchEvent::Status(status) => *status,
            other => panic!("expected status event, got {other:?}"),
        }
    }

    async fn emit_test_boot<Promote>(
        store: &SessionStore,
        session_id: &str,
        expected_proof: &str,
        promote: Promote,
    ) -> LaunchSessionRecord
    where
        Promote: FnOnce(Option<supervisor::ProcessControlHandle>) -> std::io::Result<&'static str>,
    {
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        emit_test_log(
            store,
            session_id,
            "stdout".to_string(),
            "[Render thread/INFO]: LWJGL Version: 3.3.3".to_string(),
            promote,
        )
        .await;
        assert_eq!(recv_status(&mut receiver).await.state, "running");
        assert!(matches!(
            receiver.recv().await.expect("boot log"),
            LaunchEvent::Log(_)
        ));
        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(
            stored
                .priority
                .as_ref()
                .and_then(|priority| priority.promotion.as_deref()),
            Some(expected_proof)
        );
        assert!(stored.boot_completed_at_ms.is_some());
        stored
    }

    async fn emit_test_boot_reply(
        store: &SessionStore,
        session_id: &str,
        expected_proof: &str,
        reply: supervisor::ProcessPriorityReply,
    ) -> LaunchSessionRecord {
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("process attempt");
        store
            .emit_log_for_attempt_with(
                session_id,
                &attempt,
                RawLogLine {
                    source: "stdout".to_string(),
                    text: "[Render thread/INFO]: LWJGL Version: 3.3.3".to_string(),
                    observed_at_ms: now_ms(),
                },
                prepare_log_line,
                |_| std::future::ready(reply),
            )
            .await;
        assert_eq!(recv_status(&mut receiver).await.state, "running");
        assert!(matches!(
            receiver.recv().await.expect("boot log"),
            LaunchEvent::Log(_)
        ));
        let stored = store.get(session_id).await.expect("stored record");
        assert_eq!(
            stored
                .priority
                .as_ref()
                .and_then(|priority| priority.promotion.as_deref()),
            Some(expected_proof)
        );
        assert!(stored.boot_completed_at_ms.is_some());
        stored
    }

    async fn emit_test_log<Promote>(
        store: &SessionStore,
        session_id: &str,
        source: String,
        text: String,
        promote: Promote,
    ) where
        Promote: FnOnce(Option<supervisor::ProcessControlHandle>) -> std::io::Result<&'static str>,
    {
        emit_test_log_with_prepare(store, session_id, source, text, prepare_log_line, promote)
            .await;
    }

    async fn emit_test_log_with_prepare<Prepare, Promote>(
        store: &SessionStore,
        session_id: &str,
        source: String,
        text: String,
        prepare: Prepare,
        promote: Promote,
    ) where
        Prepare: FnOnce(&str, String, String, u64) -> PreparedLogLine,
        Promote: FnOnce(Option<supervisor::ProcessControlHandle>) -> std::io::Result<&'static str>,
    {
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("process attempt");
        store
            .emit_log_for_attempt_with(
                session_id,
                &attempt,
                RawLogLine {
                    source,
                    text,
                    observed_at_ms: now_ms(),
                },
                prepare,
                |process| {
                    std::future::ready(supervisor::ProcessPriorityReply::Completed(promote(
                        process,
                    )))
                },
            )
            .await;
    }

    async fn replace_session(store: &SessionStore, session_id: &str) -> Arc<ProcessAttemptScope> {
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let stale_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("stale attempt");
        let mut replacement = test_record(session_id);
        replacement.state = LaunchState::Starting;
        store.insert(replacement).await.expect("insert session");
        stale_attempt
    }

    struct AttachedTestProcess {
        pid: u32,
        control: supervisor::ProcessControlHandle,
    }

    #[cfg(unix)]
    struct ReapGatedTestProcess {
        control: supervisor::ProcessControlHandle,
        reap_reached: tokio::sync::oneshot::Receiver<()>,
        release_reap: tokio::sync::oneshot::Sender<()>,
    }

    async fn attach_test_child(
        store: &Arc<SessionStore>,
        session_id: &str,
        mut command: Command,
    ) -> AttachedTestProcess {
        command.kill_on_drop(true);
        let child = command.spawn().expect("test child");
        let pid = child.id().expect("test child pid");
        let (control, owner) = supervisor::prepare_process_owner(child);
        let mut active_processes = store.active_processes.lock().await;
        let mut sessions = store.sessions.write().await;
        let entry = sessions.get_mut(session_id).expect("session entry");
        entry.record.pid = Some(pid);
        entry.process = Some(control.clone());
        active_processes.insert(entry.attempt.id, control.clone());
        let attempt = entry.attempt.clone();
        let output_pumps = supervisor::spawn_output_tasks(
            store.clone(),
            session_id.to_string(),
            attempt.clone(),
            None,
            None,
        );
        owner.spawn(store.clone(), session_id.to_string(), attempt, output_pumps);
        drop(sessions);
        drop(active_processes);
        AttachedTestProcess { pid, control }
    }

    #[cfg(unix)]
    async fn attach_reap_gated_test_child(
        store: &Arc<SessionStore>,
        session_id: &str,
    ) -> ReapGatedTestProcess {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("reap-gated child");
        let (control, mut owner) = supervisor::prepare_process_owner(child);
        let (reap_reached, reap_reached_rx) = tokio::sync::oneshot::channel();
        let (release_reap, release_reap_rx) = tokio::sync::oneshot::channel();
        owner.set_reap_gate(reap_reached, release_reap_rx);
        let mut active_processes = store.active_processes.lock().await;
        let mut sessions = store.sessions.write().await;
        let entry = sessions.get_mut(session_id).expect("session entry");
        entry.process = Some(control.clone());
        active_processes.insert(entry.attempt.id, control.clone());
        let attempt = entry.attempt.clone();
        let output_pumps = supervisor::spawn_output_tasks(
            store.clone(),
            session_id.to_string(),
            attempt.clone(),
            None,
            None,
        );
        owner.spawn(store.clone(), session_id.to_string(), attempt, output_pumps);
        drop(sessions);
        drop(active_processes);
        ReapGatedTestProcess {
            control,
            reap_reached: reap_reached_rx,
            release_reap,
        }
    }

    #[cfg(unix)]
    fn test_pid_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("axial-{label}-{}-{}", std::process::id(), now_ms()))
    }

    #[cfg(unix)]
    async fn wait_for_pid_file(path: &std::path::Path) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(pid) = std::fs::read_to_string(path)
                && let Ok(pid) = pid.parse()
            {
                return pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "spawned process did not publish its pid"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[cfg(unix)]
    async fn assert_process_exits(pid: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while process_is_live(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "process {pid} remained live after cancellation"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[cfg(unix)]
    fn process_is_live(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("inspect process state");
        let state = String::from_utf8_lossy(&output.stdout);
        output.status.success() && !state.trim().is_empty() && !state.trim_start().starts_with('Z')
    }

    async fn set_failure_observed_at(store: &SessionStore, session_id: &str, observed_at_ms: u64) {
        store
            .sessions
            .write()
            .await
            .get_mut(session_id)
            .expect("session entry")
            .observed_failure = Some(ObservedFailureSignal {
            class: LaunchFailureClass::JvmUnsupportedOption,
            observed_at_ms,
        });
    }

    async fn mark_stop_requested(store: &SessionStore, session_id: &str) {
        let mut sessions = store.sessions.write().await;
        let entry = sessions
            .get_mut(session_id)
            .expect("session should exist for stop intent");
        entry.stop_requested = true;
    }

    async fn wait_for_user_stop_intent(store: &SessionStore, session_id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store
                    .sessions
                    .read()
                    .await
                    .get(session_id)
                    .is_some_and(|entry| entry.stop_requested)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("user-stop intent publication");
    }

    async fn user_stop_evidence_count(store: &SessionStore, session_id: &str) -> usize {
        store
            .get(session_id)
            .await
            .expect("session")
            .stages
            .iter()
            .flat_map(|stage| &stage.evidence)
            .filter(|evidence| evidence.id.contains("process_stop_requested"))
            .count()
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
}
