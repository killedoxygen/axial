mod classify;
mod supervisor;

use croopor_launcher::{
    LaunchEvent, LaunchFailure, LaunchFailureClass, LaunchLogEvent, LaunchSessionRecord,
    LaunchState, LaunchStatusEvent,
};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, broadcast};

struct SessionEntry {
    record: LaunchSessionRecord,
    events: broadcast::Sender<LaunchEvent>,
    child: Option<Arc<Mutex<Child>>>,
    startup_observed: Arc<AtomicBool>,
    boot_completed: Arc<AtomicBool>,
    log_count: Arc<AtomicUsize>,
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

    pub async fn insert(&self, record: LaunchSessionRecord) {
        let (events, _) = broadcast::channel(256);
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
            if entry.record.state == LaunchState::Starting {
                entry.record.state = LaunchState::Running;
                let _ = entry.events.send(LaunchEvent::Status(LaunchStatusEvent {
                    state: "running".to_string(),
                    pid: entry.record.pid,
                    exit_code: None,
                    failure_class: None,
                    failure_detail: None,
                    healing: entry.record.healing.clone(),
                }));
            }
            if classify::boot_marker_detected(&text) {
                entry.boot_completed.store(true, Ordering::Relaxed);
            }
            let failure_class = classify::classify_failure_text(&text);
            if failure_class != LaunchFailureClass::Unknown {
                entry.record.failure = Some(LaunchFailure {
                    class: failure_class,
                    detail: Some(text.clone()),
                });
            }
            let _ = entry
                .events
                .send(LaunchEvent::Log(LaunchLogEvent { source, text }));
        }
    }

    pub async fn emit_status(&self, session_id: &str, event: LaunchStatusEvent) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
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
        record.pid = child.id();
        record.state = LaunchState::Starting;

        let session_id = record.session_id.0.clone();
        let child_handle = Arc::new(Mutex::new(child));
        let mut startup_observed = Arc::new(AtomicBool::new(false));
        {
            let mut sessions = self.sessions.write().await;
            if let Some(entry) = sessions.get_mut(&session_id) {
                record.failure = None;
                entry.record = record.clone();
                entry.child = Some(child_handle.clone());
                startup_observed = entry.startup_observed.clone();
                entry.startup_observed.store(false, Ordering::Relaxed);
                entry.boot_completed.store(false, Ordering::Relaxed);
                entry.log_count.store(0, Ordering::Relaxed);
            } else {
                let (events, _) = broadcast::channel(256);
                sessions.insert(
                    session_id.clone(),
                    SessionEntry {
                        record: record.clone(),
                        events,
                        child: Some(child_handle.clone()),
                        startup_observed: startup_observed.clone(),
                        boot_completed: Arc::new(AtomicBool::new(false)),
                        log_count: Arc::new(AtomicUsize::new(0)),
                    },
                );
            }
        }

        self.emit_status(
            &session_id,
            LaunchStatusEvent {
                state: "starting".to_string(),
                pid: record.pid,
                exit_code: None,
                failure_class: None,
                failure_detail: None,
                healing: record.healing.clone(),
            },
        )
        .await;

        supervisor::spawn_output_tasks(self.clone(), session_id.clone(), child_handle.clone()).await;
        supervisor::spawn_startup_watchdog(self.clone(), session_id.clone(), child_handle.clone(), startup_observed);
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

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}
