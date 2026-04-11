use croopor_launcher::{
    LaunchEvent, LaunchFailure, LaunchFailureClass, LaunchLogEvent, LaunchSessionRecord,
    LaunchState, LaunchStatusEvent, cleanup_natives_dir,
};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
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
            if boot_marker_detected(&text) {
                entry.boot_completed.store(true, Ordering::Relaxed);
            }
            let failure_class = classify_failure_text(&text);
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
            entry.record.state = parse_launch_state(&event.state);
            if event.pid.is_some() {
                entry.record.pid = event.pid;
            }
            if event.exit_code.is_some() {
                entry.record.exit_code = event.exit_code;
            }
            if let Some(failure_class) = &event.failure_class {
                entry.record.failure = Some(LaunchFailure {
                    class: parse_failure_class(failure_class),
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
        self.insert(record.clone()).await;
        let child_handle = Arc::new(Mutex::new(child));
        let mut startup_observed = Arc::new(AtomicBool::new(false));
        {
            let mut sessions = self.sessions.write().await;
            if let Some(entry) = sessions.get_mut(&session_id) {
                entry.child = Some(child_handle.clone());
                startup_observed = entry.startup_observed.clone();
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

        self.spawn_output_tasks(session_id.clone(), child_handle.clone())
            .await;
        self.spawn_startup_watchdog(session_id.clone(), child_handle.clone(), startup_observed);
        self.spawn_wait_task(session_id, child_handle);

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

    pub async fn has_active_instance(&self, instance_id: &str) -> bool {
        self.sessions.read().await.values().any(|entry| {
            entry.record.instance_id == instance_id && !is_terminal_state(entry.record.state)
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
                    && !is_terminal_state(entry.record.state)
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
            if is_terminal_state(state) {
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

    async fn spawn_output_tasks(self: &Arc<Self>, session_id: String, child: Arc<Mutex<Child>>) {
        let mut locked = child.lock().await;
        let stdout = locked.stdout.take();
        let stderr = locked.stderr.take();
        drop(locked);

        if let Some(stdout) = stdout {
            let store = self.clone();
            let session_id_clone = session_id.clone();
            tokio::spawn(async move {
                pump_output(store, session_id_clone, "stdout", stdout).await;
            });
        }
        if let Some(stderr) = stderr {
            let store = self.clone();
            tokio::spawn(async move {
                pump_output(store, session_id, "stderr", stderr).await;
            });
        }
    }

    fn spawn_wait_task(self: &Arc<Self>, session_id: String, child: Arc<Mutex<Child>>) {
        let store = self.clone();
        tokio::spawn(async move {
            let status = child.lock().await.wait().await;
            let existing = store.get(&session_id).await;
            if existing.as_ref().is_some_and(|record| {
                record.state == LaunchState::Exited && record.failure.is_some()
            }) {
                if let Some(natives_dir) = existing.and_then(|record| record.natives_dir) {
                    let _ = cleanup_natives_dir(std::path::Path::new(&natives_dir));
                }
                return;
            }
            match status {
                Ok(status) => {
                    store
                        .emit_status(
                            &session_id,
                            LaunchStatusEvent {
                                state: "exited".to_string(),
                                pid: None,
                                exit_code: status.code(),
                                failure_class: None,
                                failure_detail: None,
                                healing: None,
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
                            },
                        )
                        .await;
                }
            }
            if let Some(natives_dir) = store
                .get(&session_id)
                .await
                .and_then(|record| record.natives_dir)
            {
                let _ = cleanup_natives_dir(std::path::Path::new(&natives_dir));
            }
        });
    }

    fn spawn_startup_watchdog(
        self: &Arc<Self>,
        session_id: String,
        child: Arc<Mutex<Child>>,
        startup_observed: Arc<AtomicBool>,
    ) {
        let store = self.clone();
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
                    },
                )
                .await;
        });
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
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

fn parse_launch_state(state: &str) -> LaunchState {
    match state {
        "planning" => LaunchState::Planning,
        "validating" => LaunchState::Validating,
        "preparing" => LaunchState::Preparing,
        "starting" => LaunchState::Starting,
        "monitoring" => LaunchState::Monitoring,
        "running" => LaunchState::Running,
        "degraded" => LaunchState::Degraded,
        "failed" => LaunchState::Failed,
        "exited" => LaunchState::Exited,
        _ => LaunchState::Idle,
    }
}

fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

fn parse_failure_class(raw: &str) -> LaunchFailureClass {
    match raw {
        "jvm_unsupported_option" => LaunchFailureClass::JvmUnsupportedOption,
        "jvm_experimental_unlock_required" => LaunchFailureClass::JvmExperimentalUnlock,
        "jvm_option_ordering" => LaunchFailureClass::JvmOptionOrdering,
        "java_runtime_mismatch" => LaunchFailureClass::JavaRuntimeMismatch,
        "classpath_or_module_conflict" => LaunchFailureClass::ClasspathModuleConflict,
        "auth_mode_incompatible" => LaunchFailureClass::AuthModeIncompatible,
        "loader_bootstrap_failure" => LaunchFailureClass::LoaderBootstrapFailure,
        "startup_stalled" => LaunchFailureClass::StartupStalled,
        _ => LaunchFailureClass::Unknown,
    }
}

fn boot_marker_detected(text: &str) -> bool {
    const BOOT_MARKERS: [&str; 3] = ["Setting user:", "LWJGL Version", "[Render thread"];
    BOOT_MARKERS.iter().any(|marker| text.contains(marker))
}

fn classify_failure_text(text: &str) -> LaunchFailureClass {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return LaunchFailureClass::Unknown;
    }
    if lower.contains("unrecognized vm option") || lower.contains("unsupported vm option") {
        return LaunchFailureClass::JvmUnsupportedOption;
    }
    if lower.contains("must be enabled via -xx:+unlockexperimentalvmoptions") {
        return LaunchFailureClass::JvmExperimentalUnlock;
    }
    if lower.contains("unlock option must precede") || lower.contains("must precede") {
        return LaunchFailureClass::JvmOptionOrdering;
    }
    if lower.contains("unsupportedclassversionerror")
        || lower.contains("compiled by a more recent version of the java runtime")
        || lower.contains("requires java")
        || lower.contains("java runtime")
    {
        return LaunchFailureClass::JavaRuntimeMismatch;
    }
    if lower.contains("resolutionexception: modules")
        || lower.contains("export package")
        || lower.contains("modulelayerhandler.buildlayer")
    {
        return LaunchFailureClass::ClasspathModuleConflict;
    }
    if lower.contains("bootstraplauncher")
        || lower.contains("modlauncher")
        || lower.contains("nosuchelementexception: no value present")
    {
        return LaunchFailureClass::LoaderBootstrapFailure;
    }
    if lower.contains("microsoft account")
        || lower.contains("check your microsoft account")
        || lower.contains("multiplayer is disabled")
    {
        return LaunchFailureClass::AuthModeIncompatible;
    }
    LaunchFailureClass::Unknown
}
