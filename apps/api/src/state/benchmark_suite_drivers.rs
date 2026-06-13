use crate::logging::timestamp_utc;
use croopor_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tokio::sync::{Mutex, watch};
use tracing::warn;

const MAX_DRIVER_ERROR_CHARS: usize = 160;
const DRIVER_ID_PREFIX: &str = "benchmark-suite-driver-";
const INTERRUPTED_BY_RESTART_ERROR: &str = "driver interrupted by restart";
const AUTOMATIC_RESUME_QUEUED_ERROR: &str = "driver automatic resume queued after restart";
const AUTOMATIC_RESUME_STARTED_ERROR: &str = "driver automatic resume started after restart";
const AUTOMATIC_RESUME_LIMIT_ERROR: &str = "driver ignored after restart resume limit";
const MAX_DRIVER_FILENAME_STEM: usize = 96;
const MAX_RESUMABLE_DRIVERS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteDriverStatus {
    pub id: String,
    pub suite_id: String,
    pub mode: String,
    pub state: String,
    pub interval_ms: u64,
    pub run_count: usize,
    pub launched_run_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_run_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteDriverSuiteSummary {
    pub run_count: usize,
    pub launched_run_count: usize,
    pub pending_run_index: Option<usize>,
}

#[derive(Debug)]
pub struct BenchmarkSuiteDriverStart {
    pub status: BenchmarkSuiteDriverStatus,
    pub stop_rx: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchmarkSuiteDriverConflict;

struct BenchmarkSuiteDriverEntry {
    status: BenchmarkSuiteDriverStatus,
    stop_tx: watch::Sender<bool>,
}

#[derive(Default)]
struct BenchmarkSuiteDriverInner {
    next_id: u64,
    drivers: HashMap<String, BenchmarkSuiteDriverEntry>,
    active_by_suite: HashMap<String, String>,
    pending_resume_ids: Vec<String>,
}

pub struct BenchmarkSuiteDriverStore {
    inner: Mutex<BenchmarkSuiteDriverInner>,
    storage_dir: Option<PathBuf>,
}

impl BenchmarkSuiteDriverStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BenchmarkSuiteDriverInner::default()),
            storage_dir: None,
        }
    }

    pub fn load_from_paths(paths: &AppPaths) -> Self {
        let storage_dir = driver_dir(paths);
        let (inner, interrupted) = load_persisted_driver_inner(&storage_dir);
        for status in interrupted {
            if let Err(error) = persist_status_to_dir(&storage_dir, &status) {
                warn!(
                    driver_id = %status.id,
                    error = %error,
                    "failed to persist interrupted benchmark suite driver status during load"
                );
            }
        }

        Self {
            inner: Mutex::new(inner),
            storage_dir: Some(storage_dir),
        }
    }

    pub async fn start(
        &self,
        suite_id: String,
        mode: String,
        interval_ms: u64,
        summary: BenchmarkSuiteDriverSuiteSummary,
    ) -> Result<BenchmarkSuiteDriverStart, BenchmarkSuiteDriverConflict> {
        let start = {
            let mut inner = self.inner.lock().await;
            if let Some(existing_id) = inner.active_by_suite.get(&suite_id)
                && inner
                    .drivers
                    .get(existing_id)
                    .map(|entry| is_non_terminal(&entry.status.state))
                    .unwrap_or(false)
            {
                return Err(BenchmarkSuiteDriverConflict);
            }

            inner.next_id = inner.next_id.saturating_add(1);
            let id = format!("{DRIVER_ID_PREFIX}{:016x}", inner.next_id);
            let now = timestamp_utc();
            let (stop_tx, stop_rx) = watch::channel(false);
            let status = BenchmarkSuiteDriverStatus {
                id: id.clone(),
                suite_id: suite_id.clone(),
                mode,
                state: "scheduled".to_string(),
                interval_ms,
                run_count: summary.run_count,
                launched_run_count: summary.launched_run_count,
                pending_run_index: summary.pending_run_index,
                active_session_id: None,
                last_run_index: None,
                last_session_id: None,
                error: None,
                created_at: now.clone(),
                updated_at: now,
            };
            inner.drivers.insert(
                id.clone(),
                BenchmarkSuiteDriverEntry {
                    status: status.clone(),
                    stop_tx,
                },
            );
            inner.active_by_suite.insert(suite_id, id);

            BenchmarkSuiteDriverStart { status, stop_rx }
        };
        self.persist_transition(&start.status);
        Ok(start)
    }

    pub async fn get(&self, id: &str) -> Option<BenchmarkSuiteDriverStatus> {
        self.inner
            .lock()
            .await
            .drivers
            .get(id)
            .map(|entry| entry.status.clone())
    }

    pub async fn take_restart_interrupted_resumable_drivers(
        &self,
    ) -> Vec<BenchmarkSuiteDriverStatus> {
        let (drivers, transitions) = {
            let mut inner = self.inner.lock().await;
            let ids = std::mem::take(&mut inner.pending_resume_ids);
            let mut drivers = Vec::new();
            let mut transitions = Vec::new();
            for id in ids {
                let Some(entry) = inner.drivers.get_mut(&id) else {
                    continue;
                };
                if !is_restart_interrupted_driver(&entry.status) {
                    continue;
                }
                drivers.push(entry.status.clone());
                entry.status.error = Some(AUTOMATIC_RESUME_QUEUED_ERROR.to_string());
                entry.status.updated_at = timestamp_utc();
                transitions.push(entry.status.clone());
            }
            (drivers, transitions)
        };
        for status in transitions {
            self.persist_transition(&status);
        }

        drivers
    }

    pub async fn record_restart_resume_started(&self, id: &str) {
        self.update_restart_resume_consumed_error(id, AUTOMATIC_RESUME_STARTED_ERROR.to_string())
            .await;
    }

    pub async fn record_restart_resume_failed(&self, id: &str, error: &str) {
        let error = sanitize_driver_error(error);
        self.update_restart_resume_consumed_error(
            id,
            format!("driver automatic resume failed: {error}"),
        )
        .await;
    }

    pub async fn stop(&self, id: &str) -> Option<BenchmarkSuiteDriverStatus> {
        let status = {
            let mut inner = self.inner.lock().await;
            let entry = inner.drivers.get_mut(id)?;
            let _ = entry.stop_tx.send(true);
            let was_non_terminal = is_non_terminal(&entry.status.state);
            if is_non_terminal(&entry.status.state) {
                entry.status.state = "stopped".to_string();
                entry.status.updated_at = timestamp_utc();
            }
            let suite_id = entry.status.suite_id.clone();
            let status = entry.status.clone();
            if was_non_terminal
                && inner
                    .active_by_suite
                    .get(&suite_id)
                    .map(|active_id| active_id == id)
                    .unwrap_or(false)
            {
                inner.active_by_suite.remove(&suite_id);
            }
            status
        };
        self.persist_transition(&status);
        Some(status)
    }

    pub async fn list_recent(&self, limit: usize) -> Vec<BenchmarkSuiteDriverStatus> {
        let mut drivers = self
            .inner
            .lock()
            .await
            .drivers
            .values()
            .map(|entry| entry.status.clone())
            .collect::<Vec<_>>();
        drivers.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        drivers.truncate(limit);
        drivers
    }

    pub async fn record_active(
        &self,
        id: &str,
        summary: BenchmarkSuiteDriverSuiteSummary,
        active_session_id: Option<String>,
    ) {
        self.update_non_terminal(id, |status| {
            status.state = "active".to_string();
            apply_summary(status, summary);
            status.active_session_id = active_session_id;
            status.error = None;
        })
        .await;
    }

    pub async fn record_launched(
        &self,
        id: &str,
        summary: BenchmarkSuiteDriverSuiteSummary,
        run_index: usize,
        session_id: Option<String>,
    ) {
        self.update_non_terminal(id, |status| {
            status.state = "launched_next".to_string();
            apply_summary(status, summary);
            status.active_session_id = None;
            status.last_run_index = Some(run_index);
            status.last_session_id = session_id;
            status.error = None;
        })
        .await;
    }

    pub async fn record_complete(&self, id: &str, summary: BenchmarkSuiteDriverSuiteSummary) {
        self.update_terminal(id, "complete", None, Some(summary))
            .await;
    }

    pub async fn record_failed(&self, id: &str, error: &str) {
        self.update_terminal(id, "failed", Some(sanitize_driver_error(error)), None)
            .await;
    }

    pub async fn record_stopped(&self, id: &str) {
        self.update_terminal(id, "stopped", None, None).await;
    }

    async fn update_non_terminal(
        &self,
        id: &str,
        update: impl FnOnce(&mut BenchmarkSuiteDriverStatus),
    ) {
        let status = {
            let mut inner = self.inner.lock().await;
            let Some(entry) = inner.drivers.get_mut(id) else {
                return;
            };
            if !is_non_terminal(&entry.status.state) {
                return;
            }
            update(&mut entry.status);
            entry.status.updated_at = timestamp_utc();
            entry.status.clone()
        };
        self.persist_transition(&status);
    }

    async fn update_terminal(
        &self,
        id: &str,
        state: &str,
        error: Option<String>,
        summary: Option<BenchmarkSuiteDriverSuiteSummary>,
    ) {
        let status = {
            let mut inner = self.inner.lock().await;
            let Some(entry) = inner.drivers.get_mut(id) else {
                return;
            };
            if !is_non_terminal(&entry.status.state) {
                return;
            }
            if let Some(summary) = summary {
                apply_summary(&mut entry.status, summary);
            }
            entry.status.state = state.to_string();
            entry.status.active_session_id = None;
            entry.status.error = error;
            entry.status.updated_at = timestamp_utc();
            let suite_id = entry.status.suite_id.clone();
            let status = entry.status.clone();
            inner.active_by_suite.remove(&suite_id);
            status
        };
        self.persist_transition(&status);
    }

    async fn update_restart_resume_consumed_error(&self, id: &str, error: String) {
        let status = {
            let mut inner = self.inner.lock().await;
            let Some(entry) = inner.drivers.get_mut(id) else {
                return;
            };
            if entry.status.state != "interrupted"
                || !matches!(
                    entry.status.error.as_deref(),
                    Some(AUTOMATIC_RESUME_QUEUED_ERROR) | Some(AUTOMATIC_RESUME_STARTED_ERROR)
                )
            {
                return;
            }
            entry.status.error = Some(sanitize_driver_error(&error));
            entry.status.updated_at = timestamp_utc();
            entry.status.clone()
        };
        self.persist_transition(&status);
    }

    fn persist_transition(&self, status: &BenchmarkSuiteDriverStatus) {
        let Some(storage_dir) = &self.storage_dir else {
            return;
        };
        if let Err(error) = persist_status_to_dir(storage_dir, status) {
            warn!(
                driver_id = %status.id,
                error = %error,
                "failed to persist benchmark suite driver status"
            );
        }
    }
}

impl Default for BenchmarkSuiteDriverStore {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_summary(
    status: &mut BenchmarkSuiteDriverStatus,
    summary: BenchmarkSuiteDriverSuiteSummary,
) {
    status.run_count = summary.run_count;
    status.launched_run_count = summary.launched_run_count;
    status.pending_run_index = summary.pending_run_index;
}

fn is_non_terminal(state: &str) -> bool {
    !matches!(state, "complete" | "failed" | "stopped" | "interrupted")
}

fn is_restart_interrupted_driver(status: &BenchmarkSuiteDriverStatus) -> bool {
    status.state == "interrupted" && status.error.as_deref() == Some(INTERRUPTED_BY_RESTART_ERROR)
}

fn load_persisted_driver_inner(
    storage_dir: &Path,
) -> (BenchmarkSuiteDriverInner, Vec<BenchmarkSuiteDriverStatus>) {
    let mut inner = BenchmarkSuiteDriverInner::default();
    let mut interrupted = Vec::new();
    let entries = match fs::read_dir(storage_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (inner, interrupted),
        Err(error) => {
            warn!(
                path = %storage_dir.display(),
                error = %error,
                "failed to read benchmark suite driver status directory"
            );
            return (inner, interrupted);
        }
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let mut status = match load_status_file(&path) {
            Ok(status) => status,
            Err(error) => {
                warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to load benchmark suite driver status"
                );
                continue;
            }
        };
        if !is_safe_driver_id(&status.id) {
            warn!("skipping persisted benchmark suite driver with unsafe id");
            continue;
        }
        inner.next_id = inner
            .next_id
            .max(driver_id_index(&status.id).unwrap_or_default());
        if let Some(error) = status.error.take() {
            status.error = Some(sanitize_driver_error(&error));
        }
        let mut should_persist = false;
        if is_non_terminal(&status.state) {
            status.state = "interrupted".to_string();
            status.active_session_id = None;
            status.error = Some(INTERRUPTED_BY_RESTART_ERROR.to_string());
            status.updated_at = timestamp_utc();
            should_persist = true;
        }
        if is_restart_interrupted_driver(&status) {
            if inner.pending_resume_ids.len() < MAX_RESUMABLE_DRIVERS {
                inner.pending_resume_ids.push(status.id.clone());
            } else {
                status.error = Some(AUTOMATIC_RESUME_LIMIT_ERROR.to_string());
                status.updated_at = timestamp_utc();
                should_persist = true;
            }
        }
        if should_persist {
            interrupted.push(status.clone());
        }
        let (stop_tx, _stop_rx) = watch::channel(!is_non_terminal(&status.state));
        inner.drivers.insert(
            status.id.clone(),
            BenchmarkSuiteDriverEntry { status, stop_tx },
        );
    }

    (inner, interrupted)
}

fn load_status_file(path: &Path) -> io::Result<BenchmarkSuiteDriverStatus> {
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn persist_status_to_dir(
    storage_dir: &Path,
    status: &BenchmarkSuiteDriverStatus,
) -> io::Result<()> {
    fs::create_dir_all(storage_dir)?;
    let path = driver_path(storage_dir, &status.id);
    let data = serde_json::to_string_pretty(status)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file(&temp_path, &path)
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }
    if destination.exists() {
        let _ = fs::remove_file(destination);
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(source);
            Err(error)
        }
    }
}

fn driver_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("benchmarks").join("suite-drivers")
}

fn driver_path(storage_dir: &Path, driver_id: &str) -> PathBuf {
    storage_dir.join(safe_driver_filename(driver_id))
}

fn safe_driver_filename(driver_id: &str) -> String {
    let mut stem = driver_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .take(MAX_DRIVER_FILENAME_STEM)
        .collect::<String>();
    stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        "driver.json".to_string()
    } else {
        format!("{stem}.json")
    }
}

fn is_safe_driver_id(driver_id: &str) -> bool {
    driver_id_index(driver_id).is_some()
}

fn driver_id_index(driver_id: &str) -> Option<u64> {
    let suffix = driver_id.strip_prefix(DRIVER_ID_PREFIX)?;
    if suffix.len() != 16 || !suffix.chars().all(|value| value.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(suffix, 16).ok()
}

pub fn sanitize_driver_error(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "driver error".to_string();
    }

    let lower = value.to_ascii_lowercase();
    let sensitive = [
        "command",
        "java_path",
        "java path",
        "jvm",
        "username",
        "filesystem",
        "args",
    ];
    if sensitive.iter().any(|token| lower.contains(token))
        || value.contains('/')
        || value.contains('\\')
    {
        return "driver error".to_string();
    }

    let sanitized = value
        .chars()
        .filter(|value| !value.is_control() && !matches!(value, '/' | '\\' | ';'))
        .take(MAX_DRIVER_ERROR_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        "driver error".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn start_conflicts_for_non_terminal_suite_driver() {
        let store = BenchmarkSuiteDriverStore::new();
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };

        store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("first driver should start");
        let conflict = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await;

        assert_eq!(conflict.err(), Some(BenchmarkSuiteDriverConflict));
    }

    #[tokio::test]
    async fn stopped_driver_reports_stopped_and_allows_new_driver() {
        let store = BenchmarkSuiteDriverStore::new();
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let started = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver should start");

        let stopped = store.stop(&started.status.id).await.expect("driver status");

        assert_eq!(stopped.state, "stopped");
        assert_eq!(
            store
                .get(&started.status.id)
                .await
                .expect("stored status")
                .state,
            "stopped"
        );
        store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("terminal driver should not conflict");
    }

    #[tokio::test]
    async fn stopping_terminal_driver_does_not_clear_new_active_driver() {
        let store = BenchmarkSuiteDriverStore::new();
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let first = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("first driver should start");
        store.record_stopped(&first.status.id).await;
        let _second = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("second driver should start");

        let stopped_first = store
            .stop(&first.status.id)
            .await
            .expect("terminal driver should remain visible");
        let conflict = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await;

        assert_eq!(stopped_first.state, "stopped");
        assert_eq!(conflict.err(), Some(BenchmarkSuiteDriverConflict));
    }

    #[tokio::test]
    async fn unknown_driver_status_is_missing() {
        let store = BenchmarkSuiteDriverStore::new();

        assert!(store.get("missing").await.is_none());
        assert!(store.stop("missing").await.is_none());
    }

    #[tokio::test]
    async fn persisted_driver_status_survives_restart_and_interrupts_active_driver() {
        let root = test_root("restart-interrupt");
        let paths = test_paths(&root);
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let started = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver starts");
        store
            .record_active(
                &started.status.id,
                summary.clone(),
                Some("session-1".to_string()),
            )
            .await;

        let path = driver_path(&driver_dir(&paths), &started.status.id);
        assert!(path.is_file());
        let persisted = load_status_file(&path).expect("persisted status should load");
        assert_eq!(persisted.state, "active");

        let reloaded = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let interrupted = reloaded
            .get(&started.status.id)
            .await
            .expect("loaded interrupted driver");
        assert_eq!(interrupted.state, "interrupted");
        assert_eq!(
            interrupted.error.as_deref(),
            Some(INTERRUPTED_BY_RESTART_ERROR)
        );
        assert_eq!(interrupted.active_session_id, None);
        let rewritten = load_status_file(&path).expect("rewritten status should load");
        assert_eq!(rewritten.state, "interrupted");

        let pending = reloaded.take_restart_interrupted_resumable_drivers().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, started.status.id);
        assert_eq!(
            reloaded
                .take_restart_interrupted_resumable_drivers()
                .await
                .len(),
            0
        );

        let next = reloaded
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("interrupted driver should not conflict");
        assert_eq!(next.status.id, "benchmark-suite-driver-0000000000000002");

        cleanup(&root);
    }

    #[tokio::test]
    async fn restart_resume_queue_skips_terminal_and_manual_interrupted_drivers() {
        let root = test_root("resume-skip-terminal");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        for (index, state, error) in [
            (1, "stopped", None),
            (2, "failed", Some("manual failure")),
            (3, "complete", None),
            (4, "interrupted", Some("driver stopped by user")),
        ] {
            let status = status_fixture(index, state, error);
            fs::write(
                driver_path(&dir, &status.id),
                serde_json::to_string_pretty(&status).expect("serialize driver"),
            )
            .expect("write driver");
        }

        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);

        assert!(
            store
                .take_restart_interrupted_resumable_drivers()
                .await
                .is_empty()
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn restart_resume_queue_is_capped() {
        let root = test_root("resume-cap");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let total = MAX_RESUMABLE_DRIVERS + 3;
        for index in 1..=total {
            let status = status_fixture(index as u64, "active", None);
            fs::write(
                driver_path(&dir, &status.id),
                serde_json::to_string_pretty(&status).expect("serialize driver"),
            )
            .expect("write driver");
        }

        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let pending = store.take_restart_interrupted_resumable_drivers().await;
        let limited = store
            .list_recent(total)
            .await
            .into_iter()
            .filter(|status| status.error.as_deref() == Some(AUTOMATIC_RESUME_LIMIT_ERROR))
            .count();

        assert_eq!(pending.len(), MAX_RESUMABLE_DRIVERS);
        assert_eq!(limited, total - MAX_RESUMABLE_DRIVERS);
        cleanup(&root);
    }

    #[tokio::test]
    async fn persisted_terminal_driver_status_remains_visible_after_restart() {
        let root = test_root("terminal-visible");
        let paths = test_paths(&root);
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 1,
            pending_run_index: Some(1),
        };
        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let started = store
            .start(
                "suite-dev".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver starts");
        store.record_complete(&started.status.id, summary).await;

        let reloaded = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let status = reloaded
            .get(&started.status.id)
            .await
            .expect("loaded complete driver");

        assert_eq!(status.state, "complete");
        assert_eq!(status.error, None);

        cleanup(&root);
    }

    #[test]
    fn persisted_driver_with_unknown_fields_is_not_loaded() {
        let root = test_root("unknown-field");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let path = driver_path(&dir, "benchmark-suite-driver-0000000000000001");
        fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "benchmark-suite-driver-0000000000000001",
                "suite_id": "suite-dev",
                "mode": "development",
                "state": "complete",
                "interval_ms": 30000,
                "run_count": 1,
                "launched_run_count": 1,
                "unexpected_state": true,
                "created_at": "2026-01-01T00:00:00.000Z",
                "updated_at": "2026-01-01T00:01:00.000Z"
            }))
            .expect("serialize driver"),
        )
        .expect("write driver");

        let (inner, interrupted) = load_persisted_driver_inner(&dir);

        assert!(inner.drivers.is_empty());
        assert!(interrupted.is_empty());
        cleanup(&root);
    }

    #[test]
    fn driver_status_path_uses_sanitized_local_filename() {
        let root = test_root("safe-filename");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        let path = driver_path(&dir, "../../secret\\driver;id");
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("filename");

        assert!(path.starts_with(&dir));
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(!filename.contains('/'));
        assert!(!filename.contains('\\'));
        assert!(!filename.contains(';'));
        assert!(filename.ends_with(".json"));

        cleanup(&root);
    }

    #[test]
    fn driver_error_sanitizer_bounds_and_removes_sensitive_shapes() {
        let error = sanitize_driver_error(
            "failed command java_path /home/secret/.minecraft --jvm-args username Secret",
        );
        let lower = error.to_ascii_lowercase();

        assert_eq!(error, "driver error");
        assert!(error.len() <= MAX_DRIVER_ERROR_CHARS);
        assert!(!error.contains('/'));
        assert!(!error.contains('\\'));
        assert!(!lower.contains("command"));
        assert!(!lower.contains("java_path"));
        assert!(!lower.contains("jvm"));
        assert!(!lower.contains("username"));
        assert!(!lower.contains("args"));

        let long = "x".repeat(MAX_DRIVER_ERROR_CHARS + 32);
        assert_eq!(sanitize_driver_error(&long).len(), MAX_DRIVER_ERROR_CHARS);
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "croopor-suite-driver-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn status_fixture(index: u64, state: &str, error: Option<&str>) -> BenchmarkSuiteDriverStatus {
        BenchmarkSuiteDriverStatus {
            id: format!("benchmark-suite-driver-{index:016x}"),
            suite_id: format!("suite-{index}"),
            mode: "development".to_string(),
            state: state.to_string(),
            interval_ms: 30_000,
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
            active_session_id: Some(format!("session-{index}")),
            last_run_index: None,
            last_session_id: None,
            error: error.map(str::to_string),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:01:00.000Z".to_string(),
        }
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
