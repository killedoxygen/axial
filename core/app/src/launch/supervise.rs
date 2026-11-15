//! The session process actor. It spawns the validated command, drains its
//! output, observes startup and stop, then settles the contained tree, the
//! prepared natives and the durable outcome.
//!
//! While a healthy game runs the actor wakes only for events: output, leader
//! exit, stop, cancellation and the one startup deadline. Settlement retries
//! back off and never give up: an unresolved tree, output or record stays
//! owned and visible until it settles.

use super::{
    logs::{LogStream, MAX_OUTPUT_CHUNK_BYTES},
    prepare::PreparedSession,
    session::{STARTUP_OBSERVATION_TIMEOUT, SessionEntry, SessionPhase, SettledProcess, now_ms},
};
use crate::{
    instances::directory::RegisteredInstance, tasks::CancellationToken,
    telemetry::TelemetryErrorKind,
};
use axial_minecraft::process::{OwnedProcess, ProcessOutput, SpawnError};
use std::{process::ExitStatus, sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    process::{ChildStderr, ChildStdout, Command},
    sync::watch,
};

/// Settlement becomes visibly unresolved after this long; it still continues.
pub(super) const SETTLEMENT_DEADLINE: Duration = Duration::from_secs(5);
const SETTLE_RETRY_FIRST: Duration = Duration::from_millis(10);
const SETTLE_RETRY_LONGEST: Duration = Duration::from_secs(1);

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

pub(super) async fn run(
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
    if let Err(error) = prepared.validate_before_spawn() {
        tracing::warn!(
            stage = "input_revalidation",
            cause = ?error,
            "Accepted launch could not start."
        );
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
        Err(SpawnError::BeforeSpawn(error)) => {
            tracing::warn!(
                stage = "process_spawn",
                error_kind = ?error.kind(),
                os_error_code = error.raw_os_error(),
                "Accepted launch could not start."
            );
            entry
                .telemetry
                .failure(Some(TelemetryErrorKind::LaunchSpawnFailed));
            finish_without_process(&entry, false, prepared.natives()).await;
            sentinel.settled = true;
            return;
        }
        Err(SpawnError::Unsettled(process, output, error)) => {
            tracing::warn!(
                stage = "process_containment",
                error_kind = ?error.kind(),
                os_error_code = error.raw_os_error(),
                "Accepted launch process could not be contained."
            );
            (process, output, true)
        }
    };
    supervise(
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

/// Both pipes with heap buffers retained across every await.
struct Output {
    stdout: ChildStdout,
    stderr: ChildStderr,
    stdout_buffer: Box<[u8]>,
    stderr_buffer: Box<[u8]>,
    stdout_open: bool,
    stderr_open: bool,
    failed: bool,
}

impl Output {
    fn new(output: ProcessOutput) -> Self {
        Self {
            stdout: output.stdout,
            stderr: output.stderr,
            stdout_buffer: vec![0; MAX_OUTPUT_CHUNK_BYTES].into_boxed_slice(),
            stderr_buffer: vec![0; MAX_OUTPUT_CHUNK_BYTES].into_boxed_slice(),
            stdout_open: true,
            stderr_open: true,
            failed: false,
        }
    }

    fn drained(&self) -> bool {
        !self.stdout_open && !self.stderr_open && !self.failed
    }

    fn open(&self) -> bool {
        self.stdout_open || self.stderr_open
    }

    /// Reads the next chunk from either open pipe into the session log.
    /// Pending forever once both pipes are closed. Cancel-safe.
    async fn next(&mut self, entry: &SessionEntry) {
        let Self {
            stdout,
            stderr,
            stdout_buffer,
            stderr_buffer,
            stdout_open,
            stderr_open,
            failed,
        } = self;
        tokio::select! {
            read = stdout.read(stdout_buffer), if *stdout_open => {
                record(entry, LogStream::Stdout, read, stdout_buffer, stdout_open, failed);
            }
            read = stderr.read(stderr_buffer), if *stderr_open => {
                record(entry, LogStream::Stderr, read, stderr_buffer, stderr_open, failed);
            }
            else => std::future::pending().await,
        }
    }
}

fn record(
    entry: &SessionEntry,
    stream: LogStream,
    read: std::io::Result<usize>,
    buffer: &[u8],
    open: &mut bool,
    failed: &mut bool,
) {
    match read {
        Ok(0) => {
            *open = false;
            entry.output(stream, None);
        }
        Ok(count) => entry.output(stream, Some(&buffer[..count])),
        Err(_) => {
            *open = false;
            *failed = true;
        }
    }
}

/// How the live phase ended.
#[derive(Clone, Copy, Default)]
struct Ending {
    stop_requested: bool,
    startup_stalled: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn supervise(
    mut process: OwnedProcess,
    output: ProcessOutput,
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
    let started = tokio::time::Instant::now();
    entry.lock().process_started = Some(started);
    let pid = process.pid();
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
    let launched_at = entry.current().launched_at;
    let mut output = Output::new(output);
    let mut startup_observed = false;
    let mut status: Option<ExitStatus> = None;
    let mut ending = Ending::default();

    // Live: only events wake this loop.
    let startup_deadline = started + STARTUP_OBSERVATION_TIMEOUT;
    let mut deadline_passed = false;
    let mut stop_open = true;
    if !spawn_failed {
        loop {
            if *stop.borrow() || cancellation.is_cancelled() {
                ending.stop_requested = true;
                break;
            }
            if output.failed {
                break;
            }
            if !startup_observed {
                let (boot, has_output) = entry.startup_evidence();
                // The retained runner accepts a boot marker at once, or a live
                // process with ordinary output after its observation window;
                // a silent process is stalled at that deadline.
                if entry
                    .telemetry
                    .observe_startup(boot, has_output, started.elapsed())
                {
                    startup_observed = true;
                    record_recency(&instance, &launched_at).await;
                } else if deadline_passed && !has_output {
                    ending.startup_stalled = true;
                    break;
                }
            }
            entry.publish(|snapshot| {
                snapshot.phase = if startup_observed {
                    SessionPhase::Running
                } else {
                    SessionPhase::Starting
                };
            });
            tokio::select! {
                changed = stop.changed(), if stop_open => stop_open = changed.is_ok(),
                () = cancellation.cancelled() => {}
                waited = process.wait() => {
                    status = waited.ok();
                    break;
                }
                () = output.next(&entry) => {}
                () = tokio::time::sleep_until(startup_deadline), if !startup_observed && !deadline_passed => {
                    deadline_passed = true;
                }
            }
        }
    }
    settle(
        process,
        output,
        spawn_failed,
        &entry,
        natives,
        status,
        startup_observed,
        ending,
    )
    .await;
}

async fn record_recency(instance: &Option<RegisteredInstance>, launched_at: &str) {
    let Some(instance) = instance.clone() else {
        return;
    };
    let launched_at = launched_at.to_owned();
    if !matches!(
        tokio::task::spawn_blocking(move || instance.record_successful_launch(&launched_at)).await,
        Ok(Ok(()))
    ) {
        // Recency is best effort. Its atomic failure leaves no file effect.
        tracing::warn!("Successful launch recency could not be saved.");
    }
}

/// Terminates and reaps the whole tree, drains both pipes, settles natives
/// and persists the outcome, retrying with bounded backoff until all settle.
#[allow(clippy::too_many_arguments)]
async fn settle(
    mut process: OwnedProcess,
    mut output: Output,
    spawn_failed: bool,
    entry: &SessionEntry,
    natives: Option<Arc<crate::install::vanilla::PreparedNatives>>,
    mut status: Option<ExitStatus>,
    mut startup_observed: bool,
    ending: Ending,
) {
    let settling_since = tokio::time::Instant::now();
    let mut retry = SETTLE_RETRY_FIRST;
    let mut settlement_failed = false;
    loop {
        if !startup_observed && !spawn_failed && !ending.stop_requested && !ending.startup_stalled {
            // A boot marker may arrive in output drained after the leader exit.
            let (boot, has_output) = entry.startup_evidence();
            startup_observed = boot
                && entry
                    .telemetry
                    .observe_startup(boot, has_output, settling_since.elapsed());
        }
        let _ = process.terminate();
        if status.is_none() {
            status = process.try_wait().ok().flatten();
        }
        let tree_settled = process.tree_settled().unwrap_or(false);
        let drained = output.drained();
        let unresolved =
            output.failed || settlement_failed || settling_since.elapsed() >= SETTLEMENT_DEADLINE;
        entry.publish(|snapshot| {
            snapshot.exit_code = status.and_then(|status| status.code());
            snapshot.tree_settled = tree_settled;
            snapshot.process_alive = !tree_settled;
            snapshot.output_drained = drained;
            snapshot.phase = if unresolved {
                SessionPhase::Unresolved
            } else if ending.stop_requested {
                SessionPhase::Stopping
            } else {
                SessionPhase::Settling
            };
        });
        if status.is_some() && tree_settled && drained {
            entry.telemetry.failure(
                (!spawn_failed && !ending.stop_requested)
                    .then_some(TelemetryErrorKind::LaunchStartupFailed),
            );
            let outcome = if super::prepare::try_settle_natives(natives.clone()).await {
                entry.persist_settlement(
                    SettledProcess::ChildExited {
                        exit_code: status.and_then(|status| status.code()),
                        signal: status.and_then(exit_signal),
                        stop_requested: ending.stop_requested,
                        was_running: startup_observed,
                        spawn_failed,
                    },
                    ending.startup_stalled,
                )
            } else {
                None
            };
            match outcome {
                Some(outcome) => {
                    entry.lock().logs.clear_secrets();
                    entry.publish(|snapshot| {
                        snapshot.phase = SessionPhase::Exited;
                        snapshot.stop_allowed = false;
                        snapshot.outcome = Some(outcome);
                    });
                    return;
                }
                None => {
                    settlement_failed = true;
                    entry.publish(|snapshot| {
                        snapshot.phase = SessionPhase::Unresolved;
                        snapshot.outcome = None;
                    });
                }
            }
        }
        // Pipes are drained as fast as they produce; only the tree, native
        // and record checks are retried, with bounded backoff.
        if output.open() {
            tokio::select! {
                () = output.next(entry) => continue,
                () = tokio::time::sleep(retry) => {}
            }
        } else {
            tokio::time::sleep(retry).await;
        }
        retry = (retry * 2).min(SETTLE_RETRY_LONGEST);
    }
}

pub(super) async fn finish_without_process(
    entry: &SessionEntry,
    stopped: bool,
    natives: Option<Arc<crate::install::vanilla::PreparedNatives>>,
) {
    entry.telemetry.failure(None);
    entry.output(LogStream::Stdout, None);
    entry.output(LogStream::Stderr, None);
    let mut retry = SETTLE_RETRY_FIRST;
    let unresolved = |snapshot: &mut super::session::SessionSnapshot| {
        snapshot.phase = SessionPhase::Unresolved;
        snapshot.tree_settled = true;
        snapshot.process_alive = false;
        snapshot.output_drained = true;
        snapshot.outcome = None;
    };
    while !super::prepare::try_settle_natives(natives.clone()).await {
        entry.publish(unresolved);
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(SETTLE_RETRY_LONGEST);
    }
    let outcome = loop {
        if let Some(outcome) = entry.persist_settlement(SettledProcess::NoChild { stopped }, false)
        {
            break outcome;
        }
        entry.publish(unresolved);
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(SETTLE_RETRY_LONGEST);
    };
    entry.lock().logs.clear_secrets();
    entry.publish(|snapshot| {
        snapshot.phase = SessionPhase::Exited;
        snapshot.stop_allowed = false;
        snapshot.process_alive = false;
        snapshot.tree_settled = true;
        snapshot.output_drained = true;
        snapshot.outcome = Some(outcome);
    });
}

#[cfg(unix)]
fn exit_signal(status: ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&status)
}

#[cfg(not(unix))]
fn exit_signal(_status: ExitStatus) -> Option<i32> {
    None
}
