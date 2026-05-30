use crate::types::{CompositionState, CompositionTier, OwnershipClass};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

const LOCK_FILE_NAME: &str = ".croopor-lock.json";
const STATE_DIR_NAME: &str = ".croopor-performance";
const ROLLBACK_DIR_NAME: &str = "rollback";
const ROLLBACK_FILE_NAME: &str = "latest.json";
const ROLLBACK_FILES_DIR_NAME: &str = "files";
const ROLLBACK_HISTORY_DIR_NAME: &str = "history";
const ROLLBACK_SCHEMA_VERSION: i32 = 1;
const ROLLBACK_HISTORY_LIMIT: usize = 5;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to read state: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse state: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid performance state filename: {0}")]
    InvalidFilename(String),
    #[error("invalid performance artifact ownership for {filename}: {ownership_class}")]
    InvalidOwnership {
        filename: String,
        ownership_class: String,
    },
    #[error("invalid performance rollback snapshot id")]
    InvalidRollbackId,
    #[error("invalid rollback snapshot: {0}")]
    InvalidRollback(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSnapshot {
    pub id: String,
    pub schema_version: i32,
    pub created_at: String,
    pub state: CompositionState,
    #[serde(default)]
    pub artifacts: Vec<RollbackArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSnapshotSummary {
    pub id: String,
    pub created_at: String,
    pub composition_id: String,
    pub tier: CompositionTier,
    pub installed_count: usize,
    pub latest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackArtifact {
    pub filename: String,
    pub stored_filename: String,
}

pub fn load_state(instance_mods_dir: &Path) -> Result<Option<CompositionState>, StateError> {
    let path = lock_file_path(instance_mods_dir);
    match fs::read_to_string(path) {
        Ok(data) => {
            let state = serde_json::from_str(&data)?;
            validate_state(&state)?;
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StateError::Read(error)),
    }
}

pub fn save_state(instance_mods_dir: &Path, state: &CompositionState) -> Result<(), StateError> {
    validate_state(state)?;
    fs::create_dir_all(instance_mods_dir)?;
    let data = serde_json::to_string_pretty(state)?;
    let path = lock_file_path(instance_mods_dir);
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file_atomic(&temp_path, &path)?;
    Ok(())
}

pub fn remove_state(instance_mods_dir: &Path) -> Result<(), StateError> {
    let path = lock_file_path(instance_mods_dir);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

pub fn lock_file_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir.join(LOCK_FILE_NAME)
}

pub fn save_rollback_snapshot(
    instance_mods_dir: &Path,
    state: &CompositionState,
) -> Result<RollbackSnapshot, StateError> {
    validate_state(state)?;

    let files_dir = rollback_files_dir_path(instance_mods_dir);
    fs::create_dir_all(&files_dir)?;

    let snapshot_id = new_rollback_snapshot_id();
    let mut artifacts = Vec::new();

    for (index, installed) in state.installed_mods.iter().enumerate() {
        let source_path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
        if !source_path.is_file() {
            continue;
        }

        let stored_filename = format!("{snapshot_id}-{index}.bin");
        validate_managed_filename(&stored_filename)?;
        fs::copy(&source_path, files_dir.join(&stored_filename))?;
        artifacts.push(RollbackArtifact {
            filename: installed.filename.clone(),
            stored_filename,
        });
    }

    let snapshot = RollbackSnapshot {
        id: snapshot_id.clone(),
        schema_version: ROLLBACK_SCHEMA_VERSION,
        created_at: Utc::now().to_rfc3339(),
        state: state.clone(),
        artifacts,
    };
    write_rollback_snapshot(
        &rollback_history_file_path(instance_mods_dir, &snapshot_id),
        &snapshot,
    )?;
    write_rollback_snapshot(&rollback_file_path(instance_mods_dir), &snapshot)?;
    prune_rollback_history(instance_mods_dir)?;
    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    cleanup_unreferenced_rollback_artifacts(instance_mods_dir, &snapshots);
    Ok(snapshot)
}

pub fn load_rollback_snapshot(
    instance_mods_dir: &Path,
) -> Result<Option<RollbackSnapshot>, StateError> {
    let path = rollback_file_path(instance_mods_dir);
    let snapshot = match fs::read_to_string(path) {
        Ok(data) => serde_json::from_str::<RollbackSnapshot>(&data)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::Read(error)),
    };
    validate_rollback_snapshot(&snapshot)?;
    Ok(Some(snapshot))
}

pub fn load_rollback_snapshot_by_id(
    instance_mods_dir: &Path,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    validate_rollback_snapshot_id(snapshot_id)?;
    let path = rollback_history_file_path(instance_mods_dir, snapshot_id);
    let snapshot = match fs::read_to_string(path) {
        Ok(data) => serde_json::from_str::<RollbackSnapshot>(&data)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::Read(error)),
    };
    if snapshot.id != snapshot_id {
        return Err(StateError::InvalidRollback(
            "snapshot id does not match history filename".to_string(),
        ));
    }
    validate_rollback_snapshot(&snapshot)?;
    Ok(Some(snapshot))
}

pub fn list_rollback_snapshots(
    instance_mods_dir: &Path,
) -> Result<Vec<RollbackSnapshotSummary>, StateError> {
    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    Ok(snapshots
        .into_iter()
        .map(|record| RollbackSnapshotSummary {
            id: record.snapshot.id,
            created_at: record.snapshot.created_at,
            composition_id: record.snapshot.state.composition_id,
            tier: record.snapshot.state.tier,
            installed_count: record.snapshot.state.installed_mods.len(),
            latest: record.latest,
        })
        .collect())
}

pub fn restore_rollback_snapshot(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<CompositionState, StateError> {
    validate_rollback_snapshot(snapshot)?;

    let snapshot_filenames: HashSet<String> = snapshot
        .state
        .installed_mods
        .iter()
        .map(|installed| installed.filename.clone())
        .collect();

    if let Ok(Some(current_state)) = load_state(instance_mods_dir) {
        validate_state(&current_state)?;
        for installed in current_state.installed_mods {
            if snapshot_filenames.contains(&installed.filename) {
                continue;
            }
            let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(StateError::Read(error)),
            }
        }
    }

    let files_dir = rollback_files_dir_path(instance_mods_dir);
    for artifact in &snapshot.artifacts {
        let source_path = files_dir.join(&artifact.stored_filename);
        if !source_path.is_file() {
            return Err(StateError::InvalidRollback(format!(
                "missing rollback artifact {}",
                artifact.stored_filename
            )));
        }
        let final_path = managed_artifact_path(instance_mods_dir, &artifact.filename)?;
        let temp_path = final_path.with_extension("rollback.tmp");
        fs::copy(&source_path, &temp_path)?;
        replace_file_atomic(&temp_path, &final_path)?;
    }

    save_state(instance_mods_dir, &snapshot.state)?;
    Ok(snapshot.state.clone())
}

pub fn managed_artifact_path(
    instance_mods_dir: &Path,
    filename: &str,
) -> Result<PathBuf, StateError> {
    validate_managed_filename(filename)?;
    Ok(instance_mods_dir.join(filename))
}

fn rollback_dir_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir
        .join(STATE_DIR_NAME)
        .join(ROLLBACK_DIR_NAME)
}

fn rollback_file_path(instance_mods_dir: &Path) -> PathBuf {
    rollback_dir_path(instance_mods_dir).join(ROLLBACK_FILE_NAME)
}

fn rollback_files_dir_path(instance_mods_dir: &Path) -> PathBuf {
    rollback_dir_path(instance_mods_dir).join(ROLLBACK_FILES_DIR_NAME)
}

fn rollback_history_dir_path(instance_mods_dir: &Path) -> PathBuf {
    rollback_dir_path(instance_mods_dir).join(ROLLBACK_HISTORY_DIR_NAME)
}

fn rollback_history_file_path(instance_mods_dir: &Path, snapshot_id: &str) -> PathBuf {
    rollback_history_dir_path(instance_mods_dir).join(format!("{snapshot_id}.json"))
}

fn validate_rollback_snapshot(snapshot: &RollbackSnapshot) -> Result<(), StateError> {
    if snapshot.schema_version != ROLLBACK_SCHEMA_VERSION {
        return Err(StateError::InvalidRollback(format!(
            "unsupported schema version {}",
            snapshot.schema_version
        )));
    }
    validate_rollback_snapshot_id(&snapshot.id)?;
    validate_state(&snapshot.state)?;

    let state_filenames: HashSet<&str> = snapshot
        .state
        .installed_mods
        .iter()
        .map(|installed| installed.filename.as_str())
        .collect();
    for artifact in &snapshot.artifacts {
        validate_managed_filename(&artifact.filename)?;
        validate_managed_filename(&artifact.stored_filename)?;
        if !state_filenames.contains(artifact.filename.as_str()) {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} is not in the rollback state",
                artifact.filename
            )));
        }
    }

    Ok(())
}

fn validate_rollback_snapshot_id(snapshot_id: &str) -> Result<(), StateError> {
    let valid = !snapshot_id.is_empty()
        && snapshot_id.len() <= 96
        && snapshot_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'-' || value == b'_');
    if valid {
        Ok(())
    } else {
        Err(StateError::InvalidRollbackId)
    }
}

fn validate_state(state: &CompositionState) -> Result<(), StateError> {
    for installed in &state.installed_mods {
        validate_managed_filename(&installed.filename)?;
        if installed.ownership_class != OwnershipClass::CompositionManaged {
            return Err(StateError::InvalidOwnership {
                filename: installed.filename.clone(),
                ownership_class: serde_json::to_value(installed.ownership_class)
                    .ok()
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| format!("{:?}", installed.ownership_class)),
            });
        }
    }
    Ok(())
}

fn validate_managed_filename(filename: &str) -> Result<(), StateError> {
    let trimmed = filename.trim();
    if trimmed.is_empty()
        || trimmed != filename
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
    {
        return Err(StateError::InvalidFilename(filename.to_string()));
    }
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| StateError::InvalidFilename(filename.to_string()))?;
    if base != trimmed {
        return Err(StateError::InvalidFilename(filename.to_string()));
    }
    Ok(())
}

fn cleanup_unreferenced_rollback_artifacts(
    instance_mods_dir: &Path,
    snapshots: &[RollbackSnapshotRecord],
) {
    let files_dir = rollback_files_dir_path(instance_mods_dir);
    let keep: HashSet<&str> = snapshots
        .iter()
        .flat_map(|record| record.snapshot.artifacts.iter())
        .map(|artifact| artifact.stored_filename.as_str())
        .collect();
    let Ok(entries) = fs::read_dir(files_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        if !keep.contains(filename) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn write_rollback_snapshot(path: &Path, snapshot: &RollbackSnapshot) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(snapshot)?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file_atomic(&temp_path, path)?;
    Ok(())
}

fn new_rollback_snapshot_id() -> String {
    format!(
        "rb-{}-{}",
        Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| Utc::now().timestamp_millis()),
        std::process::id()
    )
}

#[derive(Debug, Clone)]
struct RollbackSnapshotRecord {
    snapshot: RollbackSnapshot,
    latest: bool,
}

fn load_retained_rollback_snapshots(
    instance_mods_dir: &Path,
) -> Result<Vec<RollbackSnapshotRecord>, StateError> {
    let mut snapshots = Vec::new();
    if let Some(snapshot) = load_rollback_snapshot(instance_mods_dir)? {
        snapshots.push(RollbackSnapshotRecord {
            snapshot,
            latest: true,
        });
    }

    let mut seen_ids: HashSet<String> = snapshots
        .iter()
        .map(|record| record.snapshot.id.clone())
        .collect();
    let history_dir = rollback_history_dir_path(instance_mods_dir);
    let entries = match fs::read_dir(history_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(snapshots),
        Err(error) => return Err(StateError::Read(error)),
    };

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(snapshot_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        validate_rollback_snapshot_id(snapshot_id)?;
        let snapshot = serde_json::from_str::<RollbackSnapshot>(&fs::read_to_string(&path)?)?;
        if snapshot.id != snapshot_id {
            return Err(StateError::InvalidRollback(
                "snapshot id does not match history filename".to_string(),
            ));
        }
        validate_rollback_snapshot(&snapshot)?;
        if seen_ids.contains(snapshot_id) {
            continue;
        }
        seen_ids.insert(snapshot_id.to_string());
        snapshots.push(RollbackSnapshotRecord {
            snapshot,
            latest: false,
        });
    }

    snapshots.sort_by(|left, right| {
        right
            .snapshot
            .created_at
            .cmp(&left.snapshot.created_at)
            .then_with(|| right.snapshot.id.cmp(&left.snapshot.id))
            .then_with(|| right.latest.cmp(&left.latest))
    });
    Ok(snapshots)
}

fn prune_rollback_history(instance_mods_dir: &Path) -> Result<(), StateError> {
    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    let keep: HashSet<String> = snapshots
        .iter()
        .map(|record| record.snapshot.id.clone())
        .take(ROLLBACK_HISTORY_LIMIT)
        .collect();
    let history_dir = rollback_history_dir_path(instance_mods_dir);
    let entries = match fs::read_dir(history_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(snapshot_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if keep.contains(snapshot_id) {
            continue;
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

fn replace_file_atomic(temp_path: &Path, final_path: &Path) -> Result<(), std::io::Error> {
    if fs::rename(temp_path, final_path).is_ok() {
        return Ok(());
    }

    if final_path.exists() {
        let _ = fs::remove_file(final_path);
    }

    match fs::rename(temp_path, final_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(temp_path);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::InstalledMod;

    #[test]
    fn rollback_history_retains_bounded_recent_snapshots() {
        let root = test_root("history-retention");
        let mut saved_ids = Vec::new();

        for index in 0..7 {
            let filename = format!("managed-{index}.jar");
            fs::write(root.join(&filename), format!("managed-{index}")).expect("write managed");
            let snapshot = save_rollback_snapshot(
                &root,
                &test_state(
                    &format!("core-{index}"),
                    vec![test_mod("sodium", &filename)],
                ),
            )
            .expect("save rollback snapshot");
            saved_ids.push(snapshot.id);
        }

        let summaries = list_rollback_snapshots(&root).expect("list rollback snapshots");
        let listed_ids = summaries
            .iter()
            .map(|summary| summary.id.clone())
            .collect::<Vec<_>>();

        assert_eq!(listed_ids.len(), ROLLBACK_HISTORY_LIMIT);
        assert!(!listed_ids.contains(&saved_ids[0]));
        assert!(!listed_ids.contains(&saved_ids[1]));
        assert!(listed_ids.contains(saved_ids.last().expect("latest id")));
        assert_eq!(summaries.iter().filter(|summary| summary.latest).count(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_specific_older_snapshot_restores_tracked_state_only() {
        let root = test_root("restore-older");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        fs::write(root.join("user.jar"), b"user-v1").expect("write user");
        let older = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save older snapshot");
        let older_id = older.id.clone();

        fs::write(root.join("managed-b.jar"), b"managed-b").expect("write managed b");
        save_state(
            &root,
            &test_state("core-b", vec![test_mod("lithium", "managed-b.jar")]),
        )
        .expect("save current state");
        save_rollback_snapshot(
            &root,
            &test_state("core-b", vec![test_mod("lithium", "managed-b.jar")]),
        )
        .expect("save newer snapshot");
        fs::write(root.join("user.jar"), b"user-v2").expect("mutate user");

        let snapshot = load_rollback_snapshot_by_id(&root, &older_id)
            .expect("load older snapshot")
            .expect("older snapshot exists");
        let restored = restore_rollback_snapshot(&root, &snapshot).expect("restore older");

        assert_eq!(restored.composition_id, "core-a");
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read managed a"),
            b"managed-a"
        );
        assert!(!root.join("managed-b.jar").exists());
        assert_eq!(
            fs::read(root.join("user.jar")).expect("read user"),
            b"user-v2"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_artifact_cleanup_keeps_all_retained_snapshot_files() {
        let root = test_root("artifact-cleanup");
        let mut snapshots = Vec::new();

        for index in 0..6 {
            let filename = format!("managed-{index}.jar");
            fs::write(root.join(&filename), format!("managed-{index}")).expect("write managed");
            snapshots.push(
                save_rollback_snapshot(
                    &root,
                    &test_state(
                        &format!("core-{index}"),
                        vec![test_mod("sodium", &filename)],
                    ),
                )
                .expect("save rollback snapshot"),
            );
        }

        let files_dir = rollback_files_dir_path(&root);
        for snapshot in snapshots.iter().skip(1) {
            for artifact in &snapshot.artifacts {
                assert!(
                    files_dir.join(&artifact.stored_filename).is_file(),
                    "retained artifact should remain"
                );
            }
        }
        for artifact in &snapshots[0].artifacts {
            assert!(
                !files_dir.join(&artifact.stored_filename).exists(),
                "pruned artifact should be removed"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_id_validation_rejects_unsafe_names() {
        let root = test_root("invalid-id");

        let error =
            load_rollback_snapshot_by_id(&root, "../latest").expect_err("invalid id should fail");

        assert!(matches!(error, StateError::InvalidRollbackId));
        assert!(!root.join("..").join("latest.json").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_missing_or_unknown_ownership() {
        let root = test_root("invalid-ownership-shape");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "sha512": ""
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            }))
            .expect("serialize state"),
        )
        .expect("write missing ownership state");
        assert!(matches!(
            load_state(&root).expect_err("missing ownership should be invalid"),
            StateError::Parse(_)
        ));

        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "plugin_managed",
                    "sha512": ""
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            }))
            .expect("serialize state"),
        )
        .expect("write unknown ownership state");
        assert!(matches!(
            load_state(&root).expect_err("unknown ownership should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_user_managed_artifacts_as_tracked_state() {
        let root = test_root("user-managed-state");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "user.jar",
                    "ownership_class": "user_managed",
                    "sha512": ""
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            }))
            .expect("serialize state"),
        )
        .expect("write user-managed state");

        let error = load_state(&root).expect_err("user-managed tracked state should fail");

        assert!(matches!(error, StateError::InvalidOwnership { .. }));
        let _ = fs::remove_dir_all(root);
    }

    fn test_state(composition_id: &str, installed_mods: Vec<InstalledMod>) -> CompositionState {
        CompositionState {
            composition_id: composition_id.to_string(),
            tier: CompositionTier::Core,
            installed_mods,
            installed_at: "2026-05-30T00:00:00Z".to_string(),
            failure_count: 0,
            last_failure: String::new(),
        }
    }

    fn test_mod(project_id: &str, filename: &str) -> InstalledMod {
        InstalledMod {
            project_id: project_id.to_string(),
            version_id: "version".to_string(),
            filename: filename.to_string(),
            ownership_class: OwnershipClass::CompositionManaged,
            sha512: String::new(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-performance-state-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }
}
