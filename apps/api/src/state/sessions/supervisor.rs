use super::{
    ProcessAttemptScope, ProcessKillReason, ProcessObservation, SessionStore, now_ms, priority,
    process_kill_stage_evidence, process_observation_stage_evidence,
};
use axial_launcher::{
    LaunchSessionExitReason, LaunchSessionOutcome, LaunchState, LaunchStatusEvent,
};
use std::future::Future;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const STARTUP_BOOT_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const OUTPUT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProcessTerminalCause {
    UserStop,
    StartupWatchdog,
    LaunchFailure,
    Replacement,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProcessTerminationAcceptance {
    Accepted(ProcessTerminalCause),
    Joined(ProcessTerminalCause),
    ProcessExited,
}

#[derive(Clone, Debug)]
struct ProcessOwnerError {
    kind: io::ErrorKind,
    message: Arc<str>,
}

impl ProcessOwnerError {
    fn from_io(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: Arc::from(error.to_string()),
        }
    }

    fn into_io(self) -> io::Error {
        io::Error::new(self.kind, self.message.to_string())
    }
}

#[derive(Clone, Debug)]
struct ProcessReap {
    cause: Option<ProcessTerminalCause>,
    result: Result<(), ProcessOwnerError>,
}

pub(super) enum ProcessPriorityReply {
    Completed(io::Result<&'static str>),
    ExitedBefore,
    ExitedAfter,
    StateUnavailable,
    StopAccepted,
}

enum ProcessControl {
    Terminate {
        cause: ProcessTerminalCause,
        accepted: oneshot::Sender<io::Result<ProcessTerminationAcceptance>>,
    },
    Promote {
        reply: oneshot::Sender<ProcessPriorityReply>,
    },
}

#[derive(Clone)]
pub(super) struct ProcessControlHandle {
    commands: mpsc::UnboundedSender<ProcessControl>,
    reaped: watch::Receiver<Option<ProcessReap>>,
    terminal_settled: watch::Receiver<bool>,
    #[cfg(test)]
    reject_next_start_kill: Arc<AtomicBool>,
    #[cfg(test)]
    completed: watch::Receiver<bool>,
}

pub(super) struct ProcessTerminationRequest {
    accepted: Option<oneshot::Receiver<io::Result<ProcessTerminationAcceptance>>>,
    acceptance: Option<ProcessTerminationAcceptance>,
    reaped: watch::Receiver<Option<ProcessReap>>,
    terminal_settled: watch::Receiver<bool>,
    #[cfg(test)]
    completed: watch::Receiver<bool>,
}

impl ProcessControlHandle {
    pub(super) fn terminate(&self, cause: ProcessTerminalCause) -> ProcessTerminationRequest {
        let (accepted, accepted_rx) = oneshot::channel();
        let accepted = self
            .commands
            .send(ProcessControl::Terminate { cause, accepted })
            .map(|()| accepted_rx)
            .ok();
        ProcessTerminationRequest {
            accepted,
            acceptance: None,
            reaped: self.reaped.clone(),
            terminal_settled: self.terminal_settled.clone(),
            #[cfg(test)]
            completed: self.completed.clone(),
        }
    }

    pub(super) async fn promote_after_boot(&self) -> ProcessPriorityReply {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(ProcessControl::Promote { reply })
            .is_err()
        {
            return if self.reaped.borrow().is_some() {
                ProcessPriorityReply::ExitedBefore
            } else {
                ProcessPriorityReply::StateUnavailable
            };
        }
        reply_rx.await.unwrap_or_else(|_| {
            if self.reaped.borrow().is_some() {
                ProcessPriorityReply::ExitedBefore
            } else {
                ProcessPriorityReply::StateUnavailable
            }
        })
    }

    #[cfg(test)]
    pub(super) fn completion_receiver(&self) -> watch::Receiver<bool> {
        self.completed.clone()
    }

    #[cfg(test)]
    pub(super) async fn wait_until_reaped(&self) {
        let mut reaped = self.reaped.clone();
        if reaped.borrow().is_none() {
            reaped.changed().await.expect("process reap signal");
        }
    }

    #[cfg(test)]
    pub(super) fn reject_next_start_kill(&self) {
        self.reject_next_start_kill.store(true, Ordering::SeqCst);
    }
}

impl ProcessTerminationRequest {
    pub(super) fn terminal_is_settled(&self) -> bool {
        *self.terminal_settled.borrow()
    }

    pub(super) async fn accepted(&mut self) -> io::Result<ProcessTerminationAcceptance> {
        if let Some(acceptance) = self.acceptance {
            return Ok(acceptance);
        }
        let accepted = match self.accepted.as_mut() {
            Some(accepted) => Some(accepted.await),
            None => None,
        };
        if accepted.is_some() {
            self.accepted = None;
        }
        let acceptance = match accepted {
            Some(accepted) => match accepted {
                Ok(accepted) => accepted?,
                Err(_) => {
                    return self.acceptance_after_owner_closed().await;
                }
            },
            None => {
                return self.acceptance_after_owner_closed().await;
            }
        };
        self.acceptance = Some(acceptance);
        Ok(acceptance)
    }

    async fn acceptance_after_owner_closed(&mut self) -> io::Result<ProcessTerminationAcceptance> {
        if self.reaped.borrow().is_none() {
            self.reaped.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process owner stopped before reporting reap",
                )
            })?;
        }
        let reaped = self.reaped.borrow().clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process owner stopped before reporting reap",
            )
        })?;
        reap_acceptance(&reaped)
    }

    pub(super) async fn reaped(&mut self) -> io::Result<ProcessTerminationAcceptance> {
        let acceptance = self.accepted().await?;
        if self.reaped.borrow().is_none() {
            self.reaped.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process owner stopped before reporting reap",
                )
            })?;
        }
        let reaped = self.reaped.borrow().clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process owner stopped before reporting reap",
            )
        })?;
        reaped.result.map_err(ProcessOwnerError::into_io)?;
        Ok(acceptance)
    }

    #[cfg(test)]
    pub(super) async fn completed(&mut self) -> io::Result<ProcessTerminationAcceptance> {
        let acceptance = self.settled().await?;
        if !*self.completed.borrow() {
            self.completed.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process owner stopped before reporting completion",
                )
            })?;
        }
        Ok(acceptance)
    }

    pub(super) async fn settled(&mut self) -> io::Result<ProcessTerminationAcceptance> {
        let acceptance = self.accepted().await?;
        if self.reaped.borrow().is_none() {
            self.reaped.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process owner stopped before reporting reap",
                )
            })?;
        }
        let reaped = self.reaped.borrow().clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process owner stopped before reporting reap",
            )
        })?;
        if !*self.terminal_settled.borrow() {
            self.terminal_settled.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process owner stopped before terminal settlement",
                )
            })?;
        }
        reaped.result.map_err(ProcessOwnerError::into_io)?;
        Ok(acceptance)
    }
}

fn reap_acceptance(reaped: &ProcessReap) -> io::Result<ProcessTerminationAcceptance> {
    reaped.result.clone().map_err(ProcessOwnerError::into_io)?;
    Ok(reaped
        .cause
        .map(ProcessTerminationAcceptance::Joined)
        .unwrap_or(ProcessTerminationAcceptance::ProcessExited))
}

pub(super) struct PreparedProcessOwner {
    child: Child,
    commands: mpsc::UnboundedReceiver<ProcessControl>,
    reaped: watch::Sender<Option<ProcessReap>>,
    terminal_settled: watch::Sender<bool>,
    #[cfg(test)]
    reject_next_start_kill: Arc<AtomicBool>,
    #[cfg(test)]
    completed: watch::Sender<bool>,
    #[cfg(test)]
    control_closed_probe: Option<oneshot::Sender<()>>,
    #[cfg(test)]
    reap_gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
}

pub(super) fn prepare_process_owner(child: Child) -> (ProcessControlHandle, PreparedProcessOwner) {
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (reaped, reaped_rx) = watch::channel(None);
    let (terminal_settled, terminal_settled_rx) = watch::channel(false);
    #[cfg(test)]
    let (completed, completed_rx) = watch::channel(false);
    #[cfg(test)]
    let reject_next_start_kill = Arc::new(AtomicBool::new(false));
    (
        ProcessControlHandle {
            commands,
            reaped: reaped_rx,
            terminal_settled: terminal_settled_rx,
            #[cfg(test)]
            reject_next_start_kill: reject_next_start_kill.clone(),
            #[cfg(test)]
            completed: completed_rx,
        },
        PreparedProcessOwner {
            child,
            commands: commands_rx,
            reaped,
            terminal_settled,
            #[cfg(test)]
            reject_next_start_kill,
            #[cfg(test)]
            completed,
            #[cfg(test)]
            control_closed_probe: None,
            #[cfg(test)]
            reap_gate: None,
        },
    )
}

#[cfg(test)]
pub(super) fn rejected_process_control_handle() -> ProcessControlHandle {
    let (commands, commands_rx) = mpsc::unbounded_channel();
    drop(commands_rx);
    let (reaped, reaped_rx) = watch::channel(None);
    drop(reaped);
    let (terminal_settled, terminal_settled_rx) = watch::channel(false);
    drop(terminal_settled);
    let (completed, completed_rx) = watch::channel(false);
    drop(completed);
    ProcessControlHandle {
        commands,
        reaped: reaped_rx,
        terminal_settled: terminal_settled_rx,
        reject_next_start_kill: Arc::new(AtomicBool::new(false)),
        completed: completed_rx,
    }
}

#[cfg(test)]
pub(super) struct GatedTerminationControl {
    pub(super) handle: ProcessControlHandle,
    commands: mpsc::UnboundedReceiver<ProcessControl>,
    pending_acceptance: Option<oneshot::Sender<io::Result<ProcessTerminationAcceptance>>>,
    reaped: watch::Sender<Option<ProcessReap>>,
    terminal_settled: watch::Sender<bool>,
    completed: watch::Sender<bool>,
}

#[cfg(test)]
impl GatedTerminationControl {
    pub(super) async fn capture_user_stop_request(&mut self) {
        let control = self.commands.recv().await.expect("termination request");
        let ProcessControl::Terminate { cause, accepted } = control else {
            panic!("expected termination request");
        };
        assert_eq!(cause, ProcessTerminalCause::UserStop);
        self.pending_acceptance = Some(accepted);
    }

    pub(super) fn accept_user_stop(&mut self) {
        self.pending_acceptance
            .take()
            .expect("captured termination acceptance")
            .send(Ok(ProcessTerminationAcceptance::Accepted(
                ProcessTerminalCause::UserStop,
            )))
            .ok();
    }

    pub(super) fn reject_user_stop(&mut self, error: io::Error) {
        self.pending_acceptance
            .take()
            .expect("captured termination acceptance")
            .send(Err(error))
            .ok();
    }

    pub(super) fn publish_user_stop_reap(&self, result: io::Result<()>) {
        self.reaped.send_replace(Some(ProcessReap {
            cause: Some(ProcessTerminalCause::UserStop),
            result: result
                .as_ref()
                .map(|_| ())
                .map_err(ProcessOwnerError::from_io),
        }));
        self.terminal_settled.send_replace(true);
        self.completed.send_replace(true);
    }
}

#[cfg(test)]
pub(super) fn gated_termination_control() -> GatedTerminationControl {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (reaped, reaped_rx) = watch::channel(None);
    let (terminal_settled, terminal_settled_rx) = watch::channel(false);
    let (completed, completed_rx) = watch::channel(false);
    GatedTerminationControl {
        handle: ProcessControlHandle {
            commands,
            reaped: reaped_rx,
            terminal_settled: terminal_settled_rx,
            reject_next_start_kill: Arc::new(AtomicBool::new(false)),
            completed: completed_rx,
        },
        commands: command_rx,
        pending_acceptance: None,
        reaped,
        terminal_settled,
        completed,
    }
}

struct PendingLogLine {
    source: &'static str,
    text: String,
    observed_at_ms: u64,
}

pub(super) struct OutputPumpTasks {
    stdout: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
    processor: JoinHandle<()>,
}

#[cfg(test)]
pub(super) fn output_pump_tasks_with_processor(processor: JoinHandle<()>) -> OutputPumpTasks {
    OutputPumpTasks {
        stdout: None,
        stderr: None,
        processor,
    }
}

impl OutputPumpTasks {
    async fn drain(self) -> Result<(), tokio::task::JoinError> {
        self.drain_with_timeout(OUTPUT_DRAIN_TIMEOUT).await
    }

    async fn drain_with_timeout(self, timeout: Duration) -> Result<(), tokio::task::JoinError> {
        tokio::join!(
            finish_reader(self.stdout, "stdout", timeout),
            finish_reader(self.stderr, "stderr", timeout),
        );
        self.processor.await
    }
}

async fn finish_reader(reader: Option<JoinHandle<()>>, source: &'static str, timeout: Duration) {
    let Some(mut reader) = reader else {
        return;
    };
    if tokio::time::timeout(timeout, &mut reader).await.is_ok() {
        return;
    }

    tracing::warn!(
        source,
        "output pipe drain exceeded bounded timeout; unread bytes were discarded"
    );
    reader.abort();
    let _ = reader.await;
}

pub(super) fn spawn_output_tasks(
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) -> OutputPumpTasks {
    let (sender, mut receiver) = mpsc::channel::<PendingLogLine>(OUTPUT_QUEUE_CAPACITY);
    let stdout = spawn_output_reader("stdout", stdout, sender.clone());
    let stderr = spawn_output_reader("stderr", stderr, sender.clone());
    drop(sender);

    let processor = tokio::spawn(async move {
        while let Some(line) = receiver.recv().await {
            store
                .emit_log_for_attempt(
                    &session_id,
                    &attempt,
                    line.source,
                    line.text,
                    line.observed_at_ms,
                )
                .await;
        }
    });

    OutputPumpTasks {
        stdout,
        stderr,
        processor,
    }
}

fn spawn_output_reader<R>(
    source: &'static str,
    output: Option<R>,
    sender: mpsc::Sender<PendingLogLine>,
) -> Option<JoinHandle<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    output.map(|output| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            loop {
                let Ok(permit) = sender.reserve().await else {
                    return;
                };
                match lines.next_line().await {
                    Ok(Some(text)) => permit.send(PendingLogLine {
                        source,
                        text,
                        observed_at_ms: now_ms(),
                    }),
                    Ok(None) | Err(_) => return,
                }
            }
        })
    })
}

impl PreparedProcessOwner {
    #[cfg(test)]
    fn set_control_closed_probe(&mut self, probe: oneshot::Sender<()>) {
        self.control_closed_probe = Some(probe);
    }

    #[cfg(test)]
    pub(super) fn set_reap_gate(
        &mut self,
        reached: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    ) {
        self.reap_gate = Some((reached, release));
    }

    pub(super) fn spawn(
        mut self,
        store: Arc<SessionStore>,
        session_id: String,
        attempt: Arc<ProcessAttemptScope>,
        output_pumps: OutputPumpTasks,
    ) {
        tokio::spawn(async move {
            let mut requested_cause = None;
            let mut control_open = true;
            let status = loop {
                if !control_open {
                    break self.child.wait().await;
                }

                tokio::select! {
                    biased;
                    status = self.child.wait() => break status,
                    control = self.commands.recv() => {
                        let Some(control) = control else {
                            control_open = false;
                            #[cfg(test)]
                            if let Some(probe) = self.control_closed_probe.take() {
                                let _ = probe.send(());
                            }
                            continue;
                        };
                        if let Some(status) = handle_process_control(
                            &mut self.child,
                            &mut requested_cause,
                            control,
                            #[cfg(test)]
                            &self.reject_next_start_kill,
                        ) {
                            break status;
                        }
                    }
                }
            };
            let exit_observed_at_ms = now_ms();

            #[cfg(test)]
            if let Some((reached, release)) = self.reap_gate.take() {
                let _ = reached.send(());
                let _ = release.await;
            }

            let reap = ProcessReap {
                cause: requested_cause,
                result: status
                    .as_ref()
                    .map(|_| ())
                    .map_err(ProcessOwnerError::from_io),
            };
            self.reaped.send_replace(Some(reap));

            self.commands.close();
            while let Some(control) = self.commands.recv().await {
                reply_after_process_exit(control, requested_cause);
            }

            if requested_cause == Some(ProcessTerminalCause::StartupWatchdog) {
                settle_watchdog_exit(store.as_ref(), &session_id, &attempt, exit_observed_at_ms)
                    .await;
                self.terminal_settled.send_replace(true);
                if let Some(error) = output_pumps.drain().await.err() {
                    trace_output_processor_error(&error);
                }
            } else {
                let output_processor_error = output_pumps.drain().await.err();
                settle_process_exit(
                    store.as_ref(),
                    &session_id,
                    &attempt,
                    status,
                    requested_cause,
                    output_processor_error,
                    exit_observed_at_ms,
                )
                .await;
                self.terminal_settled.send_replace(true);
            }
            store.process_owner_completed(attempt.id).await;
            #[cfg(test)]
            self.completed.send_replace(true);
        });
    }
}

fn handle_process_control(
    child: &mut Child,
    requested_cause: &mut Option<ProcessTerminalCause>,
    control: ProcessControl,
    #[cfg(test)] reject_next_start_kill: &AtomicBool,
) -> Option<io::Result<ExitStatus>> {
    match control {
        ProcessControl::Terminate { cause, accepted } => {
            if let Some(existing) = *requested_cause {
                let _ = accepted.send(Ok(ProcessTerminationAcceptance::Joined(existing)));
                return None;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = accepted.send(Ok(ProcessTerminationAcceptance::ProcessExited));
                    Some(Ok(status))
                }
                Ok(None) => {
                    #[cfg(test)]
                    if reject_next_start_kill.swap(false, Ordering::SeqCst) {
                        let _ = accepted.send(Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "injected start_kill rejection",
                        )));
                        return None;
                    }
                    match child.start_kill() {
                        Ok(()) => {
                            *requested_cause = Some(cause);
                            let _ =
                                accepted.send(Ok(ProcessTerminationAcceptance::Accepted(cause)));
                            None
                        }
                        Err(error) => {
                            let _ = accepted.send(Err(error));
                            match child.try_wait() {
                                Ok(Some(status)) => Some(Ok(status)),
                                Ok(None) => None,
                                Err(error) => Some(Err(error)),
                            }
                        }
                    }
                }
                Err(error) => {
                    let copied = copy_io_error(&error);
                    let _ = accepted.send(Err(error));
                    Some(Err(copied))
                }
            }
        }
        ProcessControl::Promote { reply } => {
            if requested_cause.is_some() {
                let _ = reply.send(ProcessPriorityReply::StopAccepted);
                return None;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = reply.send(ProcessPriorityReply::ExitedBefore);
                    Some(Ok(status))
                }
                Ok(None) => {
                    let promotion = priority::promote_after_boot(Some(child))
                        .map(|promotion| promotion.proof_value());
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let _ = reply.send(ProcessPriorityReply::ExitedAfter);
                            Some(Ok(status))
                        }
                        Ok(None) => {
                            let _ = reply.send(ProcessPriorityReply::Completed(promotion));
                            None
                        }
                        Err(_) => {
                            let _ = reply.send(ProcessPriorityReply::StateUnavailable);
                            None
                        }
                    }
                }
                Err(_) => {
                    let _ = reply.send(ProcessPriorityReply::StateUnavailable);
                    None
                }
            }
        }
    }
}

fn reply_after_process_exit(
    control: ProcessControl,
    requested_cause: Option<ProcessTerminalCause>,
) {
    match control {
        ProcessControl::Terminate { accepted, .. } => {
            let acceptance = requested_cause
                .map(ProcessTerminationAcceptance::Joined)
                .unwrap_or(ProcessTerminationAcceptance::ProcessExited);
            let _ = accepted.send(Ok(acceptance));
        }
        ProcessControl::Promote { reply } => {
            let _ = reply.send(ProcessPriorityReply::ExitedBefore);
        }
    }
}

fn copy_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

async fn settle_process_exit(
    store: &SessionStore,
    session_id: &str,
    attempt: &Arc<ProcessAttemptScope>,
    status: io::Result<ExitStatus>,
    requested_cause: Option<ProcessTerminalCause>,
    output_processor_error: Option<tokio::task::JoinError>,
    exit_observed_at_ms: u64,
) {
    if matches!(
        requested_cause,
        Some(
            ProcessTerminalCause::LaunchFailure
                | ProcessTerminalCause::Replacement
                | ProcessTerminalCause::Shutdown
        )
    ) {
        return;
    }

    let Some(exit_context) = store
        .process_exit_context(session_id, attempt, exit_observed_at_ms)
        .await
    else {
        return;
    };
    if matches!(
        exit_context.record.state,
        LaunchState::Failed | LaunchState::Exited
    ) && exit_context.record.failure.is_some()
    {
        return;
    }
    let (exit_code, failure_class, failure_detail, evidence) = if let Some(error) =
        output_processor_error
    {
        trace_output_processor_error(&error);
        let exit_code = match status {
            Ok(status) => status.code(),
            Err(_) => Some(-1),
        };
        (
            exit_code,
            Some("unknown".to_string()),
            Some("launch output processing failed".to_string()),
            process_observation_stage_evidence(session_id, ProcessObservation::Exited),
        )
    } else {
        match status {
            Ok(status) => {
                let exit_code = status.code();
                let evidence = exit_code
                    .map(|exit_code| {
                        process_observation_stage_evidence(
                            session_id,
                            ProcessObservation::ExitCode(exit_code),
                        )
                    })
                    .unwrap_or_else(|| {
                        process_observation_stage_evidence(session_id, ProcessObservation::Exited)
                    });
                (
                    exit_code,
                    exit_context
                        .observed_failure
                        .map(|failure| failure.as_str().to_string()),
                    None,
                    evidence,
                )
            }
            Err(error) => (
                Some(-1),
                Some("unknown".to_string()),
                Some(error.to_string()),
                process_observation_stage_evidence(session_id, ProcessObservation::Exited),
            ),
        }
    };
    store
        .emit_status_for_attempt(
            session_id,
            attempt,
            LaunchStatusEvent {
                state: "exited".to_string(),
                benchmark: None,
                pid: None,
                exit_code,
                failure_class,
                failure_detail,
                healing: exit_context.record.healing.clone(),
                guardian: exit_context.record.guardian.clone(),
                outcome: (requested_cause == Some(ProcessTerminalCause::UserStop)).then(|| {
                    LaunchSessionOutcome::from_reason(LaunchSessionExitReason::LauncherStopped)
                }),
                notice: None,
                evidence,
                stages: Vec::new(),
            },
        )
        .await;
}

async fn settle_watchdog_exit(
    store: &SessionStore,
    session_id: &str,
    attempt: &Arc<ProcessAttemptScope>,
    exit_observed_at_ms: u64,
) {
    let Some(exit_context) = store
        .process_exit_context(session_id, attempt, exit_observed_at_ms)
        .await
    else {
        return;
    };
    store
        .emit_status_for_attempt(
            session_id,
            attempt,
            LaunchStatusEvent {
                state: "exited".to_string(),
                benchmark: None,
                pid: exit_context.record.pid,
                exit_code: Some(-1),
                failure_class: Some("startup_stalled".to_string()),
                failure_detail: Some("no startup activity observed".to_string()),
                healing: exit_context.record.healing.clone(),
                guardian: exit_context.record.guardian.clone(),
                outcome: Some(LaunchSessionOutcome::from_reason(
                    LaunchSessionExitReason::WatchdogKilled,
                )),
                notice: None,
                evidence: process_kill_stage_evidence(
                    session_id,
                    ProcessKillReason::StartupWatchdog,
                ),
                stages: Vec::new(),
            },
        )
        .await;
}

fn trace_output_processor_error(error: &tokio::task::JoinError) {
    tracing::error!(
        cancelled = error.is_cancelled(),
        panicked = error.is_panic(),
        "launch output processor did not complete"
    );
}

pub(super) fn spawn_startup_watchdog(
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
) {
    drop(spawn_startup_watchdog_with_trigger(
        store,
        session_id,
        attempt,
        async {
            tokio::time::sleep(STARTUP_BOOT_WATCHDOG_TIMEOUT).await;
        },
    ));
}

pub(super) fn spawn_startup_watchdog_with_trigger<Trigger>(
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
    trigger: Trigger,
) -> JoinHandle<()>
where
    Trigger: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        trigger.await;
        let log_transition = attempt.log_transition.lock().await;
        let Some(control) = store
            .startup_watchdog_process_for_attempt(&session_id, &attempt)
            .await
        else {
            return;
        };
        let mut request = control.terminate(ProcessTerminalCause::StartupWatchdog);
        let acceptance = request.accepted().await;
        if matches!(
            acceptance,
            Ok(ProcessTerminationAcceptance::Accepted(
                ProcessTerminalCause::StartupWatchdog
            ))
        ) {
            let _ = request.settled().await;
        }
        drop(log_transition);
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_record;
    use super::*;
    use axial_launcher::{LaunchFailure, LaunchFailureClass, LaunchSessionOutcome};
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::sync::Notify;

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exit_settlement_preserves_failed_record_with_failure_evidence() {
        let store = Arc::new(SessionStore::new());
        let session_id = "preserve-guardian-failed-session";
        let mut record = test_record(session_id);
        record.state = LaunchState::Failed;
        record.failure = Some(LaunchFailure {
            class: LaunchFailureClass::Unknown,
            detail: Some("Could not record the launch recovery safely.".to_string()),
        });
        record.outcome = Some(LaunchSessionOutcome::from_reason(
            LaunchSessionExitReason::CrashedBeforeBoot,
        ));
        store.insert(record.clone()).await.expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("process attempt");
        let status = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .status()
            .await
            .expect("exit status");

        settle_process_exit(
            store.as_ref(),
            session_id,
            &attempt,
            Ok(status),
            None,
            None,
            now_ms(),
        )
        .await;

        let preserved = store.get(session_id).await.expect("preserved session");
        assert_eq!(preserved.state, LaunchState::Failed);
        assert_eq!(preserved.failure, record.failure);
        assert_eq!(preserved.outcome, record.outcome);
        assert_eq!(preserved.exit_code, record.exit_code);
    }

    #[tokio::test]
    async fn output_drain_waits_until_an_inflight_failure_line_is_committed() {
        let store = Arc::new(SessionStore::new());
        let session_id = "output-drain-inflight-failure";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("attempt");
        let release = Arc::new(Notify::new());
        let pump_store = store.clone();
        let pump_release = release.clone();
        let pump_attempt = attempt.clone();
        let processor = tokio::spawn(async move {
            pump_release.notified().await;
            pump_store
                .emit_log_for_attempt(
                    session_id,
                    &pump_attempt,
                    "stderr",
                    "Unrecognized VM option '-XX:+UseZGC'".to_string(),
                    now_ms(),
                )
                .await;
        });
        let pumps = OutputPumpTasks {
            stdout: None,
            stderr: None,
            processor,
        };

        let drain = tokio::spawn(pumps.drain());
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        assert_eq!(store.observed_failure(session_id).await, None);
        release.notify_one();
        drain
            .await
            .expect("output drain task")
            .expect("output processor");

        assert_eq!(
            store
                .process_exit_context(session_id, &attempt, now_ms())
                .await
                .and_then(|context| context.observed_failure),
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
    }

    #[tokio::test]
    async fn output_drain_bounds_stalled_readers_without_repolling_completed_readers() {
        let complete = tokio::spawn(async {});
        let stalled = tokio::spawn(std::future::pending());
        let processor = tokio::spawn(async {});
        let pumps = OutputPumpTasks {
            stdout: Some(complete),
            stderr: Some(stalled),
            processor,
        };

        tokio::time::timeout(
            Duration::from_millis(250),
            pumps.drain_with_timeout(Duration::from_millis(10)),
        )
        .await
        .expect("bounded drain")
        .expect("output processor");

        let pumps = OutputPumpTasks {
            stdout: Some(tokio::spawn(std::future::pending())),
            stderr: Some(tokio::spawn(std::future::pending())),
            processor: tokio::spawn(async {}),
        };

        tokio::time::timeout(
            Duration::from_millis(250),
            pumps.drain_with_timeout(Duration::from_millis(10)),
        )
        .await
        .expect("bounded drain")
        .expect("output processor");
    }

    #[tokio::test]
    async fn output_drain_surfaces_processor_panic() {
        let pumps = OutputPumpTasks {
            stdout: None,
            stderr: None,
            processor: tokio::spawn(async { panic!("processor failed") }),
        };

        let error = pumps
            .drain_with_timeout(Duration::from_millis(10))
            .await
            .expect_err("processor join error");

        assert!(error.is_panic());
    }

    #[tokio::test]
    async fn settled_waits_for_terminal_milestone_before_returning_reap_error() {
        let (accepted, accepted_rx) = oneshot::channel();
        let (reaped, reaped_rx) = watch::channel(None);
        let (terminal_settled, terminal_settled_rx) = watch::channel(false);
        let (completed, completed_rx) = watch::channel(false);
        let request = ProcessTerminationRequest {
            accepted: Some(accepted_rx),
            acceptance: None,
            reaped: reaped_rx,
            terminal_settled: terminal_settled_rx,
            completed: completed_rx,
        };
        let settled = tokio::spawn(async move {
            let mut request = request;
            request.settled().await
        });

        accepted
            .send(Ok(ProcessTerminationAcceptance::Accepted(
                ProcessTerminalCause::UserStop,
            )))
            .expect("publish acceptance");
        let reap_error = io::Error::other("synthetic reap failure");
        reaped.send_replace(Some(ProcessReap {
            cause: Some(ProcessTerminalCause::UserStop),
            result: Err(ProcessOwnerError::from_io(&reap_error)),
        }));
        tokio::task::yield_now().await;
        assert!(!settled.is_finished());

        terminal_settled.send_replace(true);
        let error = settled
            .await
            .expect("settlement task")
            .expect_err("reap error");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "synthetic reap failure");
        drop(completed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_termination_requests_share_reap_before_drain_and_completion() {
        let store = Arc::new(SessionStore::new());
        let attempt = ProcessAttemptScope::new(1);
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("test child");
        let (control, owner) = prepare_process_owner(child);
        let release_processor = Arc::new(Notify::new());
        let processor_release = release_processor.clone();
        let output_pumps = OutputPumpTasks {
            stdout: None,
            stderr: None,
            processor: tokio::spawn(async move { processor_release.notified().await }),
        };
        owner.spawn(
            store,
            "owner-shared-reap".to_string(),
            attempt,
            output_pumps,
        );

        let mut first = control.terminate(ProcessTerminalCause::UserStop);
        let mut second = control.terminate(ProcessTerminalCause::StartupWatchdog);
        let (first_acceptance, second_acceptance) =
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(first.reaped(), second.reaped())
            })
            .await
            .expect("shared reap deadline");

        assert_eq!(
            first_acceptance.expect("first reap"),
            ProcessTerminationAcceptance::Accepted(ProcessTerminalCause::UserStop)
        );
        assert_eq!(
            second_acceptance.expect("second reap"),
            ProcessTerminationAcceptance::Joined(ProcessTerminalCause::UserStop)
        );
        assert!(!*control.completed.borrow());

        release_processor.notify_one();
        tokio::time::timeout(Duration::from_secs(2), first.completed())
            .await
            .expect("owner completion deadline")
            .expect("owner completion");
        assert!(*control.completed.borrow());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_termination_request_still_kills_and_reaps_the_child() {
        let store = Arc::new(SessionStore::new());
        let attempt = ProcessAttemptScope::new(1);
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("test child");
        let pid = child.id().expect("test child pid");
        let (control, owner) = prepare_process_owner(child);
        let release_processor = Arc::new(Notify::new());
        let processor_release = release_processor.clone();
        owner.spawn(
            store,
            "owner-dropped-request".to_string(),
            attempt,
            OutputPumpTasks {
                stdout: None,
                stderr: None,
                processor: tokio::spawn(async move { processor_release.notified().await }),
            },
        );

        drop(control.terminate(ProcessTerminalCause::UserStop));
        let mut reaped = control.reaped.clone();
        tokio::time::timeout(Duration::from_secs(2), reaped.changed())
            .await
            .expect("reap deadline")
            .expect("reap signal");

        assert_eq!(
            reaped.borrow().as_ref().expect("reap result").cause,
            Some(ProcessTerminalCause::UserStop)
        );
        assert!(!process_is_live(pid));
        assert!(!*control.completed.borrow());

        release_processor.notify_one();
        let mut completed = control.completed.clone();
        tokio::time::timeout(Duration::from_secs(2), completed.changed())
            .await
            .expect("completion deadline")
            .expect("completion signal");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_settles_terminal_status_before_output_drain_under_log_lock() {
        let store = Arc::new(SessionStore::new());
        let session_id = "watchdog-terminal-before-drain";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await.expect("insert session");
        let attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("watchdog attempt");
        let mut receiver = store.subscribe(session_id).await.expect("subscribe");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("watchdog child");
        let (control, owner) = prepare_process_owner(child);
        let release_processor = Arc::new(Notify::new());
        let processor_release = release_processor.clone();
        owner.spawn(
            store,
            session_id.to_string(),
            attempt.clone(),
            OutputPumpTasks {
                stdout: None,
                stderr: None,
                processor: tokio::spawn(async move { processor_release.notified().await }),
            },
        );

        let log_transition = attempt.log_transition.lock().await;
        let mut request = control.terminate(ProcessTerminalCause::StartupWatchdog);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), request.settled())
                .await
                .expect("watchdog settlement deadline")
                .expect("watchdog settlement"),
            ProcessTerminationAcceptance::Accepted(ProcessTerminalCause::StartupWatchdog)
        );
        let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("watchdog status deadline")
            .expect("watchdog status");
        let super::super::LaunchEvent::Status(status) = event else {
            panic!("expected watchdog status");
        };
        assert_eq!(status.state, "exited");
        assert_eq!(
            status.outcome.as_ref().expect("watchdog outcome").reason,
            LaunchSessionExitReason::WatchdogKilled
        );
        assert!(!*control.completed.borrow());

        drop(log_transition);
        release_processor.notify_one();
        tokio::time::timeout(Duration::from_secs(2), request.completed())
            .await
            .expect("watchdog completion deadline")
            .expect("watchdog completion");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_control_channel_keeps_natural_wait_active() {
        let store = Arc::new(SessionStore::new());
        let attempt = ProcessAttemptScope::new(1);
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("IFS= read -r line")
            .stdin(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("stdin-gated child");
        let mut stdin = child.stdin.take().expect("child stdin");
        let (control, mut owner) = prepare_process_owner(child);
        let mut completed = control.completed.clone();
        let (control_closed, control_closed_rx) = oneshot::channel();
        owner.set_control_closed_probe(control_closed);
        owner.spawn(
            store,
            "owner-closed-control".to_string(),
            attempt,
            OutputPumpTasks {
                stdout: None,
                stderr: None,
                processor: tokio::spawn(async {}),
            },
        );

        drop(control);
        control_closed_rx.await.expect("control close observed");
        assert!(!*completed.borrow());

        stdin.write_all(b"continue\n").await.expect("release child");
        drop(stdin);
        tokio::time::timeout(Duration::from_secs(2), completed.changed())
            .await
            .expect("natural completion deadline")
            .expect("natural completion signal");
        assert!(*completed.borrow());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_first_exit_replies_to_queued_promotion_before_output_drain() {
        let store = Arc::new(SessionStore::new());
        let attempt = ProcessAttemptScope::new(1);
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let mut child = command.spawn().expect("test child");
        child.kill().await.expect("pre-reap child");
        let (control, owner) = prepare_process_owner(child);
        let mut completed = control.completed.clone();
        let (promotion, promotion_rx) = oneshot::channel();
        control
            .commands
            .send(ProcessControl::Promote { reply: promotion })
            .expect("queue promotion");
        let output_pumps = OutputPumpTasks {
            stdout: None,
            stderr: None,
            processor: tokio::spawn(async move {
                assert!(matches!(
                    promotion_rx.await.expect("promotion reply"),
                    ProcessPriorityReply::ExitedBefore
                ));
            }),
        };
        owner.spawn(
            store,
            "owner-queued-promotion".to_string(),
            attempt,
            output_pumps,
        );

        tokio::time::timeout(Duration::from_secs(2), completed.changed())
            .await
            .expect("queued promotion completion deadline")
            .expect("queued promotion completion");
        assert!(*completed.borrow());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_first_cached_exit_beats_queued_termination() {
        let store = Arc::new(SessionStore::new());
        let attempt = ProcessAttemptScope::new(1);
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let mut child = command.spawn().expect("test child");
        child.kill().await.expect("pre-reap child");
        let (control, owner) = prepare_process_owner(child);
        let mut request = control.terminate(ProcessTerminalCause::UserStop);
        owner.spawn(
            store,
            "owner-wait-first-terminate".to_string(),
            attempt,
            OutputPumpTasks {
                stdout: None,
                stderr: None,
                processor: tokio::spawn(async {}),
            },
        );

        assert_eq!(
            request.reaped().await.expect("natural reap"),
            ProcessTerminationAcceptance::ProcessExited
        );
        assert_eq!(
            control.reaped.borrow().as_ref().expect("reap result").cause,
            None
        );
    }

    #[cfg(all(unix, not(windows)))]
    #[tokio::test]
    async fn live_owner_executes_priority_effect_on_its_exact_child() {
        let store = Arc::new(SessionStore::new());
        let attempt = ProcessAttemptScope::new(1);
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("test child");
        let (control, owner) = prepare_process_owner(child);
        owner.spawn(
            store,
            "owner-live-promotion".to_string(),
            attempt,
            OutputPumpTasks {
                stdout: None,
                stderr: None,
                processor: tokio::spawn(async {}),
            },
        );

        let ProcessPriorityReply::Completed(promotion) = control.promote_after_boot().await else {
            panic!("expected live promotion effect");
        };
        assert_eq!(promotion.expect("priority effect"), "noop");

        let mut cleanup = control.terminate(ProcessTerminalCause::Shutdown);
        cleanup.completed().await.expect("cleanup owner");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_commits_before_superseded_owner_drain_and_removes_exact_registry_id() {
        let store = Arc::new(SessionStore::new());
        let session_id = "owner-nonblocking-replacement";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let old_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("old attempt");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("old child");
        let (control, owner) = prepare_process_owner(child);
        let mut completed = control.completed.clone();
        let release_processor = Arc::new(Notify::new());
        let processor_release = release_processor.clone();
        {
            let mut active = store.active_processes.lock().await;
            let mut sessions = store.sessions.write().await;
            sessions.get_mut(session_id).expect("old entry").process = Some(control.clone());
            active.insert(old_attempt.id, control);
            owner.spawn(
                store.clone(),
                session_id.to_string(),
                old_attempt.clone(),
                OutputPumpTasks {
                    stdout: None,
                    stderr: None,
                    processor: tokio::spawn(async move { processor_release.notified().await }),
                },
            );
        }

        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let current_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("replacement attempt");

        assert_ne!(current_attempt.id, old_attempt.id);
        assert!(!*completed.borrow());
        assert!(
            store
                .active_processes
                .lock()
                .await
                .contains_key(&old_attempt.id)
        );

        release_processor.notify_one();
        tokio::time::timeout(Duration::from_secs(2), completed.changed())
            .await
            .expect("superseded owner completion deadline")
            .expect("superseded owner completion");
        assert!(
            !store
                .active_processes
                .lock()
                .await
                .contains_key(&old_attempt.id)
        );
        assert_eq!(
            store
                .current_process_attempt(session_id)
                .await
                .expect("current attempt")
                .id,
            current_attempt.id
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_registry_reaps_a_live_superseded_owner_before_clear() {
        let store = Arc::new(SessionStore::new());
        let session_id = "shutdown-superseded-owner";
        store
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let old_attempt = store
            .current_process_attempt(session_id)
            .await
            .expect("old attempt");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30").kill_on_drop(true);
        let child = command.spawn().expect("superseded child");
        let pid = child.id().expect("superseded child pid");
        let (control, owner) = prepare_process_owner(child);
        {
            let mut active = store.active_processes.lock().await;
            let mut sessions = store.sessions.write().await;
            sessions.get_mut(session_id).expect("old entry").process = Some(control.clone());
            active.insert(old_attempt.id, control);
            owner.spawn(
                store.clone(),
                session_id.to_string(),
                old_attempt.clone(),
                OutputPumpTasks {
                    stdout: None,
                    stderr: None,
                    processor: tokio::spawn(async {}),
                },
            );

            let replacement = ProcessAttemptScope::new(store.next_attempt_id());
            let entry = sessions.get_mut(session_id).expect("replacement entry");
            entry.attempt = replacement;
            entry.process = None;
        }

        tokio::time::timeout(Duration::from_secs(2), store.terminate_all())
            .await
            .expect("shutdown superseded reap deadline")
            .expect("shutdown superseded owner");

        assert!(!process_is_live(pid));
        assert!(store.active_processes.lock().await.is_empty());
        assert!(store.sessions.read().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_awaits_successful_owners_and_preserves_sessions_after_any_rejection() {
        let store = Arc::new(SessionStore::new());
        let session_id = "shutdown-mixed-owner-results";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        let launched = store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn shutdown target");
        let pid = launched.pid.expect("shutdown target pid");
        let rejected_id = u64::MAX;
        store
            .active_processes
            .lock()
            .await
            .insert(rejected_id, rejected_process_control_handle());

        let error = tokio::time::timeout(Duration::from_secs(2), store.terminate_all())
            .await
            .expect("shutdown deadline")
            .expect_err("rejected owner must fail shutdown");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!process_is_live(pid));
        assert!(store.get(session_id).await.is_some());
        store.active_processes.lock().await.remove(&rejected_id);
        store
            .terminate_all()
            .await
            .expect("shutdown retries after rejected owner is repaired");
        assert!(store.active_processes.lock().await.is_empty());
        assert!(store.sessions.read().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_shutdown_caller_does_not_cancel_the_owned_coordinator() {
        let store = Arc::new(SessionStore::new());
        let session_id = "shutdown-caller-cancelled";
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 30");
        store
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn shutdown target");
        let control = store
            .sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.process.clone())
            .expect("shutdown control");
        let mut reaped = control.reaped.clone();
        let sessions = store.sessions.read().await;
        let shutdown_store = store.clone();
        let shutdown = tokio::spawn(async move { shutdown_store.terminate_all().await });

        tokio::time::timeout(Duration::from_secs(2), reaped.changed())
            .await
            .expect("shutdown reap deadline")
            .expect("shutdown reap signal");
        shutdown.abort();
        assert!(shutdown.await.expect_err("cancelled caller").is_cancelled());
        assert!(!sessions.is_empty());
        drop(sessions);

        let lifecycle = store.lifecycle_transition.clone().lock_owned();
        let lifecycle = tokio::time::timeout(Duration::from_secs(2), lifecycle)
            .await
            .expect("detached coordinator completion");
        assert!(store.active_processes.lock().await.is_empty());
        assert!(store.sessions.read().await.is_empty());
        drop(lifecycle);
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
}
