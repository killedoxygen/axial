use super::{
    ProcessAttemptScope, ProcessKillReason, ProcessObservation, SessionStore, now_ms,
    process_kill_stage_evidence, process_observation_stage_evidence,
};
use axial_launcher::{
    LaunchSessionExitReason, LaunchSessionOutcome, LaunchState, LaunchStatusEvent,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const STARTUP_BOOT_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const OUTPUT_QUEUE_CAPACITY: usize = 256;

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

pub(super) fn spawn_wait_task(
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
    output_pumps: OutputPumpTasks,
) {
    tokio::spawn(async move {
        let Some(child) = attempt.child.clone() else {
            return;
        };
        let status = loop {
            let try_status = {
                let mut locked = child.lock().await;
                locked.try_wait()
            };

            match try_status {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(error) => break Err(error),
            }
        };
        let exit_observed_at_ms = now_ms();
        let output_processor_error = output_pumps.drain().await.err();
        let Some(exit_context) = store
            .process_exit_context(&session_id, &attempt, exit_observed_at_ms)
            .await
        else {
            return;
        };
        if exit_context.record.state == LaunchState::Exited && exit_context.record.failure.is_some()
        {
            return;
        }
        let (exit_code, failure_class, failure_detail, evidence) =
            if let Some(error) = output_processor_error {
                tracing::error!(
                    cancelled = error.is_cancelled(),
                    panicked = error.is_panic(),
                    "launch output processor did not complete"
                );
                let exit_code = match status {
                    Ok(status) => status.code(),
                    Err(_) => Some(-1),
                };
                (
                    exit_code,
                    Some("unknown".to_string()),
                    Some("launch output processing failed".to_string()),
                    process_observation_stage_evidence(&session_id, ProcessObservation::Exited),
                )
            } else {
                match status {
                    Ok(status) => {
                        let exit_code = status.code();
                        let evidence = exit_code
                            .map(|exit_code| {
                                process_observation_stage_evidence(
                                    &session_id,
                                    ProcessObservation::ExitCode(exit_code),
                                )
                            })
                            .unwrap_or_else(|| {
                                process_observation_stage_evidence(
                                    &session_id,
                                    ProcessObservation::Exited,
                                )
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
                        process_observation_stage_evidence(&session_id, ProcessObservation::Exited),
                    ),
                }
            };
        store
            .emit_status_for_attempt(
                &session_id,
                &attempt,
                LaunchStatusEvent {
                    state: "exited".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code,
                    failure_class,
                    failure_detail,
                    healing: exit_context.record.healing.clone(),
                    guardian: exit_context.record.guardian.clone(),
                    outcome: None,
                    notice: None,
                    evidence,
                    stages: Vec::new(),
                },
            )
            .await;
    });
}

pub(super) fn spawn_startup_watchdog(
    store: Arc<SessionStore>,
    session_id: String,
    attempt: Arc<ProcessAttemptScope>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(STARTUP_BOOT_WATCHDOG_TIMEOUT).await;
        let _log_transition = attempt.log_transition.lock().await;
        let Some(child) = attempt.child.as_ref() else {
            return;
        };
        let mut child = child.lock().await;
        let Some(record) = store
            .startup_watchdog_record_for_attempt(&session_id, &attempt)
            .await
        else {
            return;
        };

        let _ = child.kill().await;
        drop(child);
        store
            .emit_status_for_attempt(
                &session_id,
                &attempt,
                LaunchStatusEvent {
                    state: "exited".to_string(),
                    benchmark: None,
                    pid: record.pid,
                    exit_code: Some(-1),
                    failure_class: Some("startup_stalled".to_string()),
                    failure_detail: Some("no startup activity observed".to_string()),
                    healing: record.healing.clone(),
                    guardian: record.guardian.clone(),
                    outcome: Some(LaunchSessionOutcome::from_reason(
                        LaunchSessionExitReason::WatchdogKilled,
                    )),
                    notice: None,
                    evidence: process_kill_stage_evidence(
                        &session_id,
                        ProcessKillReason::StartupWatchdog,
                    ),
                    stages: Vec::new(),
                },
            )
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::super::test_record;
    use super::*;
    use axial_launcher::LaunchFailureClass;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn output_drain_waits_until_an_inflight_failure_line_is_committed() {
        let store = Arc::new(SessionStore::new());
        let session_id = "output-drain-inflight-failure";
        let mut record = test_record(session_id);
        record.state = LaunchState::Starting;
        store.insert(record).await;
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
}
