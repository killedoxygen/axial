use crate::logging::timestamp_utc;
use croopor_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::warn;

pub const PERFORMANCE_OPERATION_ID_PREFIX: &str = "performance-install-";
pub const INTERRUPTED_BY_RESTART_ERROR: &str = "performance operation interrupted by restart";
const MAX_OPERATION_ERROR_CHARS: usize = 160;
const MAX_OPERATION_FILENAME_STEM: usize = 96;
const MAX_RESUMABLE_OPERATIONS: usize = 16;
const DUPLICATE_RESUME_ERROR: &str =
    "duplicate pending performance operation ignored after restart";
const RESUME_LIMIT_ERROR: &str = "pending performance operation ignored after restart resume limit";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PerformanceOperationPayload {
    pub version_id: String,
    pub instance_performance_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PerformanceOperationStatus {
    pub id: String,
    pub instance_id: String,
    pub action: String,
    pub payload: PerformanceOperationPayload,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformanceOperationConflict;

#[derive(Default)]
struct PerformanceOperationInner {
    operations: HashMap<String, PerformanceOperationStatus>,
    active_by_instance: HashMap<String, String>,
    pending_resume_ids: Vec<String>,
}

pub struct PerformanceOperationStore {
    inner: Mutex<PerformanceOperationInner>,
    storage_dir: Option<PathBuf>,
}

impl PerformanceOperationStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PerformanceOperationInner::default()),
            storage_dir: None,
        }
    }

    pub fn load_from_paths(paths: &AppPaths) -> Self {
        let storage_dir = operation_dir(paths);
        let (inner, interrupted) = load_persisted_operation_inner(&storage_dir);
        for status in interrupted {
            if let Err(error) = persist_status_to_dir(&storage_dir, &status) {
                warn!(
                    operation_id = %status.id,
                    error = %error,
                    "failed to persist interrupted performance operation status during load"
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
        instance_id: String,
        action: String,
        payload: PerformanceOperationPayload,
    ) -> Result<PerformanceOperationStatus, PerformanceOperationConflict> {
        let status = {
            let mut inner = self.inner.lock().await;
            if let Some(existing_id) = inner.active_by_instance.get(&instance_id) {
                if inner
                    .operations
                    .get(existing_id)
                    .map(|status| is_non_terminal(&status.state))
                    .unwrap_or(false)
                {
                    return Err(PerformanceOperationConflict);
                }
            }

            let id = generate_performance_operation_id();
            let now = timestamp_utc();
            let status = PerformanceOperationStatus {
                id: id.clone(),
                instance_id: instance_id.clone(),
                action,
                payload,
                state: "queued".to_string(),
                error: None,
                created_at: now.clone(),
                updated_at: now,
            };
            inner.operations.insert(id.clone(), status.clone());
            inner.active_by_instance.insert(instance_id, id);
            status
        };
        self.persist_transition(&status);
        Ok(status)
    }

    pub async fn take_pending_resumable_operations(&self) -> Vec<PerformanceOperationStatus> {
        let mut inner = self.inner.lock().await;
        let ids = std::mem::take(&mut inner.pending_resume_ids);
        ids.into_iter()
            .filter_map(|id| inner.operations.get(&id).cloned())
            .filter(|status| is_non_terminal(&status.state))
            .collect()
    }

    pub async fn get(&self, id: &str) -> Option<PerformanceOperationStatus> {
        if !is_safe_operation_id(id) {
            return None;
        }
        self.inner.lock().await.operations.get(id).cloned()
    }

    pub async fn record_progress(&self, id: &str, state: &str) {
        let status = {
            let mut inner = self.inner.lock().await;
            let Some(status) = inner.operations.get_mut(id) else {
                return;
            };
            if !is_non_terminal(&status.state) {
                return;
            }
            status.state = state.to_string();
            status.error = None;
            status.updated_at = timestamp_utc();
            status.clone()
        };
        self.persist_transition(&status);
    }

    pub async fn record_complete(&self, id: &str) {
        self.record_terminal(id, "complete", None).await;
    }

    pub async fn record_failed(&self, id: &str, error: &str) {
        self.record_terminal(id, "failed", Some(sanitize_operation_error(error)))
            .await;
    }

    async fn record_terminal(&self, id: &str, state: &str, error: Option<String>) {
        let status = {
            let mut inner = self.inner.lock().await;
            let Some(status) = inner.operations.get_mut(id) else {
                return;
            };
            if !is_non_terminal(&status.state) {
                return;
            }
            status.state = state.to_string();
            status.error = error;
            status.updated_at = timestamp_utc();
            let instance_id = status.instance_id.clone();
            let status = status.clone();
            if inner
                .active_by_instance
                .get(&instance_id)
                .map(|active_id| active_id == id)
                .unwrap_or(false)
            {
                inner.active_by_instance.remove(&instance_id);
            }
            status
        };
        self.persist_transition(&status);
    }

    fn persist_transition(&self, status: &PerformanceOperationStatus) {
        let Some(storage_dir) = &self.storage_dir else {
            return;
        };
        if let Err(error) = persist_status_to_dir(storage_dir, status) {
            warn!(
                operation_id = %status.id,
                error = %error,
                "failed to persist performance operation status"
            );
        }
    }
}

impl Default for PerformanceOperationStore {
    fn default() -> Self {
        Self::new()
    }
}

fn load_persisted_operation_inner(
    storage_dir: &Path,
) -> (PerformanceOperationInner, Vec<PerformanceOperationStatus>) {
    let mut inner = PerformanceOperationInner::default();
    let mut interrupted = Vec::new();
    let entries = match fs::read_dir(storage_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (inner, interrupted),
        Err(error) => {
            warn!(
                path = %storage_dir.display(),
                error = %error,
                "failed to read performance operation status directory"
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
                    "failed to load performance operation status"
                );
                continue;
            }
        };
        if !is_safe_operation_id(&status.id) {
            warn!("skipping persisted performance operation with unsafe id");
            continue;
        }
        if let Some(error) = status.error.take() {
            status.error = Some(sanitize_operation_error(&error));
        }
        if is_non_terminal(&status.state) {
            if !is_valid_loaded_status(&status) {
                warn!(
                    operation_id = %status.id,
                    "skipping malformed persisted performance operation"
                );
                continue;
            }
            if inner.pending_resume_ids.len() >= MAX_RESUMABLE_OPERATIONS {
                status.state = "interrupted".to_string();
                status.error = Some(RESUME_LIMIT_ERROR.to_string());
                status.updated_at = timestamp_utc();
                interrupted.push(status.clone());
            } else if inner.active_by_instance.contains_key(&status.instance_id) {
                status.state = "interrupted".to_string();
                status.error = Some(DUPLICATE_RESUME_ERROR.to_string());
                status.updated_at = timestamp_utc();
                interrupted.push(status.clone());
            } else {
                inner
                    .active_by_instance
                    .insert(status.instance_id.clone(), status.id.clone());
                inner.pending_resume_ids.push(status.id.clone());
            }
        }
        inner.operations.insert(status.id.clone(), status);
    }

    (inner, interrupted)
}

fn is_non_terminal(state: &str) -> bool {
    !matches!(state, "complete" | "failed" | "interrupted")
}

fn is_valid_loaded_status(status: &PerformanceOperationStatus) -> bool {
    matches!(status.action.as_str(), "install" | "remove" | "rollback")
        && !status.instance_id.trim().is_empty()
        && !status.payload.version_id.trim().is_empty()
}

fn load_status_file(path: &Path) -> io::Result<PerformanceOperationStatus> {
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn persist_status_to_dir(
    storage_dir: &Path,
    status: &PerformanceOperationStatus,
) -> io::Result<()> {
    fs::create_dir_all(storage_dir)?;
    let path = operation_path(storage_dir, &status.id);
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

pub fn operation_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("performance").join("operations")
}

pub fn operation_path(storage_dir: &Path, operation_id: &str) -> PathBuf {
    storage_dir.join(safe_operation_filename(operation_id))
}

fn safe_operation_filename(operation_id: &str) -> String {
    let mut stem = operation_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .take(MAX_OPERATION_FILENAME_STEM)
        .collect::<String>();
    stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        "operation.json".to_string()
    } else {
        format!("{stem}.json")
    }
}

fn is_safe_operation_id(operation_id: &str) -> bool {
    operation_id_index(operation_id).is_some()
}

fn operation_id_index(operation_id: &str) -> Option<u128> {
    let suffix = operation_id.strip_prefix(PERFORMANCE_OPERATION_ID_PREFIX)?;
    if suffix.len() != 32 || !suffix.chars().all(|value| value.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(suffix, 16).ok()
}

pub fn generate_performance_operation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{PERFORMANCE_OPERATION_ID_PREFIX}{nanos:032x}")
}

pub fn sanitize_operation_error(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "performance operation failed".to_string();
    }

    let sanitized = value
        .chars()
        .filter(|value| !value.is_control() && !matches!(value, '/' | '\\' | ';'))
        .take(MAX_OPERATION_ERROR_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        "performance operation failed".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn persisted_operation_status_survives_restart_as_pending_resume() {
        let root = test_root("restart-resume");
        let paths = test_paths(&root);
        let store = PerformanceOperationStore::load_from_paths(&paths);
        let started = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");
        store.record_progress(&started.id, "applying").await;

        let path = operation_path(&operation_dir(&paths), &started.id);
        assert!(path.is_file());
        let persisted = load_status_file(&path).expect("persisted status should load");
        assert_eq!(persisted.state, "applying");

        let reloaded = PerformanceOperationStore::load_from_paths(&paths);
        let resumable = reloaded
            .get(&started.id)
            .await
            .expect("loaded resumable operation");
        assert_eq!(resumable.state, "applying");
        assert_eq!(resumable.error, None);
        let pending = reloaded.take_pending_resumable_operations().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, started.id);
        assert!(
            reloaded
                .take_pending_resumable_operations()
                .await
                .is_empty()
        );

        let conflict = reloaded
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await;
        assert_eq!(conflict.err(), Some(PerformanceOperationConflict));
        reloaded.record_complete(&started.id).await;
        reloaded
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("completed resumed operation should not conflict");

        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_operation_status_remains_visible_after_restart() {
        let root = test_root("terminal-visible");
        let paths = test_paths(&root);
        let store = PerformanceOperationStore::load_from_paths(&paths);
        let started = store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");
        store.record_complete(&started.id).await;

        let reloaded = PerformanceOperationStore::load_from_paths(&paths);
        let status = reloaded
            .get(&started.id)
            .await
            .expect("loaded complete operation");

        assert_eq!(status.state, "complete");
        assert_eq!(status.error, None);

        cleanup(&root);
    }

    #[tokio::test]
    async fn non_terminal_same_instance_operation_conflicts_during_runtime() {
        let store = PerformanceOperationStore::new();
        store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");

        let conflict = store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await;

        assert_eq!(conflict.err(), Some(PerformanceOperationConflict));
    }

    #[tokio::test]
    async fn terminal_same_instance_operation_allows_new_work() {
        let store = PerformanceOperationStore::new();
        let started = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");
        store.record_failed(&started.id, "failed").await;

        store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("terminal operation should not conflict");
    }

    #[test]
    fn operation_status_path_uses_sanitized_local_filename() {
        let root = test_root("safe-filename");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        let path = operation_path(&dir, "../../secret\\operation;id");
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
    fn unsafe_operation_ids_are_not_loaded_or_returned() {
        let root = test_root("unsafe-id");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let status = PerformanceOperationStatus {
            id: "../../secret".to_string(),
            instance_id: "instance-a".to_string(),
            action: "install".to_string(),
            payload: test_payload(),
            state: "complete".to_string(),
            error: None,
            created_at: timestamp_utc(),
            updated_at: timestamp_utc(),
        };
        persist_status_to_dir(&dir, &status).expect("persist unsafe status");

        let (inner, interrupted) = load_persisted_operation_inner(&dir);

        assert!(inner.operations.is_empty());
        assert!(interrupted.is_empty());

        cleanup(&root);
    }

    #[test]
    fn duplicate_pending_operations_for_instance_interrupt_extra_records() {
        let root = test_root("duplicate-pending");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let first = test_status(
            "performance-install-00000000000000000000000000000001",
            "instance-a",
            "install",
            "applying",
            test_payload(),
        );
        let second = test_status(
            "performance-install-00000000000000000000000000000002",
            "instance-a",
            "remove",
            "removing",
            test_payload(),
        );
        persist_status_to_dir(&dir, &first).expect("persist first status");
        persist_status_to_dir(&dir, &second).expect("persist second status");

        let (inner, interrupted) = load_persisted_operation_inner(&dir);

        assert_eq!(inner.pending_resume_ids.len(), 1);
        assert_eq!(interrupted.len(), 1);
        assert_eq!(
            interrupted[0].error.as_deref(),
            Some(DUPLICATE_RESUME_ERROR)
        );
        assert_eq!(
            inner.active_by_instance.get("instance-a"),
            inner.pending_resume_ids.first()
        );

        cleanup(&root);
    }

    #[test]
    fn malformed_current_schema_pending_operation_is_not_resumed() {
        let root = test_root("malformed-pending");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let mut payload = test_payload();
        payload.version_id = String::new();
        let status = test_status(
            "performance-install-00000000000000000000000000000001",
            "instance-a",
            "install",
            "applying",
            payload,
        );
        persist_status_to_dir(&dir, &status).expect("persist malformed status");

        let (inner, interrupted) = load_persisted_operation_inner(&dir);

        assert!(inner.operations.is_empty());
        assert!(inner.active_by_instance.is_empty());
        assert!(inner.pending_resume_ids.is_empty());
        assert!(interrupted.is_empty());

        cleanup(&root);
    }

    #[test]
    fn operation_error_sanitizer_bounds_error() {
        let long = "x".repeat(MAX_OPERATION_ERROR_CHARS + 32);
        let error = sanitize_operation_error(&format!("failed; {long}"));

        assert!(error.len() <= MAX_OPERATION_ERROR_CHARS);
        assert!(!error.contains(';'));
        assert_eq!(sanitize_operation_error(""), "performance operation failed");
    }

    fn test_payload() -> PerformanceOperationPayload {
        PerformanceOperationPayload {
            version_id: "1.20.4-fabric".to_string(),
            instance_performance_mode: "managed".to_string(),
            game_version: None,
            loader: None,
            mode: None,
            rollback_id: None,
        }
    }

    fn test_status(
        id: &str,
        instance_id: &str,
        action: &str,
        state: &str,
        payload: PerformanceOperationPayload,
    ) -> PerformanceOperationStatus {
        PerformanceOperationStatus {
            id: id.to_string(),
            instance_id: instance_id.to_string(),
            action: action.to_string(),
            payload,
            state: state.to_string(),
            error: None,
            created_at: timestamp_utc(),
            updated_at: timestamp_utc(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "croopor-performance-operation-{name}-{}-{nanos}",
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

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
