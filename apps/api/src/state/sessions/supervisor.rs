use super::SessionStore;
use croopor_launcher::{LaunchState, LaunchStatusEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::Mutex;

pub(super) async fn spawn_output_tasks(
    store: Arc<SessionStore>,
    session_id: String,
    child: Arc<Mutex<Child>>,
) {
    let mut locked = child.lock().await;
    let stdout = locked.stdout.take();
    let stderr = locked.stderr.take();
    drop(locked);

    if let Some(stdout) = stdout {
        let store = store.clone();
        let session_id_clone = session_id.clone();
        tokio::spawn(async move {
            pump_output(store, session_id_clone, "stdout", stdout).await;
        });
    }
    if let Some(stderr) = stderr {
        let store = store.clone();
        tokio::spawn(async move {
            pump_output(store, session_id, "stderr", stderr).await;
        });
    }
}

pub(super) fn spawn_wait_task(
    store: Arc<SessionStore>,
    session_id: String,
    child: Arc<Mutex<Child>>,
) {
    tokio::spawn(async move {
        let status = child.lock().await.wait().await;
        let existing = store.get(&session_id).await;
        if existing
            .as_ref()
            .is_some_and(|record| record.state == LaunchState::Exited && record.failure.is_some())
        {
            return;
        }
        let observed_failure = store.observed_failure(&session_id).await;
        match status {
            Ok(status) => {
                store
                    .emit_status(
                        &session_id,
                        LaunchStatusEvent {
                            state: "exited".to_string(),
                            pid: None,
                            exit_code: status.code(),
                            failure_class: observed_failure
                                .as_ref()
                                .map(|failure| failure.class.as_str().to_string()),
                            failure_detail: observed_failure
                                .as_ref()
                                .and_then(|failure| failure.detail.clone()),
                            healing: None,
                            guardian: existing.as_ref().and_then(|record| record.guardian.clone()),
                        },
                    )
                    .await;
            }
            Err(error) => {
                store
                    .emit_status(
                        &session_id,
                        LaunchStatusEvent {
                            state: "exited".to_string(),
                            pid: None,
                            exit_code: Some(-1),
                            failure_class: Some("unknown".to_string()),
                            failure_detail: Some(error.to_string()),
                            healing: None,
                            guardian: existing.as_ref().and_then(|record| record.guardian.clone()),
                        },
                    )
                    .await;
            }
        }
    });
}

pub(super) fn spawn_startup_watchdog(
    store: Arc<SessionStore>,
    session_id: String,
    child: Arc<Mutex<Child>>,
    startup_observed: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if startup_observed.load(Ordering::Relaxed) {
            return;
        }

        let Some(record) = store.get(&session_id).await else {
            return;
        };
        if matches!(record.state, LaunchState::Exited | LaunchState::Failed) {
            return;
        }

        let _ = child.lock().await.kill().await;
        store
            .emit_status(
                &session_id,
                LaunchStatusEvent {
                    state: "exited".to_string(),
                    pid: record.pid,
                    exit_code: Some(-1),
                    failure_class: Some("startup_stalled".to_string()),
                    failure_detail: Some("no startup activity observed".to_string()),
                    healing: record.healing.clone(),
                    guardian: record.guardian.clone(),
                },
            )
            .await;
    });
}

async fn pump_output(
    store: Arc<SessionStore>,
    session_id: String,
    source: &'static str,
    output: impl tokio::io::AsyncRead + Unpin,
) {
    let mut lines = BufReader::new(output).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        store.emit_log(&session_id, source, line).await;
    }
}
