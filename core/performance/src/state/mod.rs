use crate::types::CompositionState;
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
const ROLLBACK_SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to read state: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse state: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid performance state filename: {0}")]
    InvalidFilename(String),
    #[error("invalid rollback snapshot: {0}")]
    InvalidRollback(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSnapshot {
    pub schema_version: i32,
    pub created_at: String,
    pub state: CompositionState,
    #[serde(default)]
    pub artifacts: Vec<RollbackArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackArtifact {
    pub filename: String,
    pub stored_filename: String,
}

pub fn load_state(instance_mods_dir: &Path) -> Result<Option<CompositionState>, StateError> {
    let path = lock_file_path(instance_mods_dir);
    match fs::read_to_string(path) {
        Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StateError::Read(error)),
    }
}

pub fn save_state(instance_mods_dir: &Path, state: &CompositionState) -> Result<(), StateError> {
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
    validate_state_filenames(state)?;

    let files_dir = rollback_files_dir_path(instance_mods_dir);
    fs::create_dir_all(&files_dir)?;

    let snapshot_id = format!(
        "{}-{}",
        Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| Utc::now().timestamp_millis()),
        std::process::id()
    );
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
        schema_version: ROLLBACK_SCHEMA_VERSION,
        created_at: Utc::now().to_rfc3339(),
        state: state.clone(),
        artifacts,
    };
    let data = serde_json::to_string_pretty(&snapshot)?;
    let path = rollback_file_path(instance_mods_dir);
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file_atomic(&temp_path, &path)?;
    cleanup_unreferenced_rollback_artifacts(instance_mods_dir, &snapshot);
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
        validate_state_filenames(&current_state)?;
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

fn validate_rollback_snapshot(snapshot: &RollbackSnapshot) -> Result<(), StateError> {
    if snapshot.schema_version != ROLLBACK_SCHEMA_VERSION {
        return Err(StateError::InvalidRollback(format!(
            "unsupported schema version {}",
            snapshot.schema_version
        )));
    }
    validate_state_filenames(&snapshot.state)?;

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

fn validate_state_filenames(state: &CompositionState) -> Result<(), StateError> {
    for installed in &state.installed_mods {
        validate_managed_filename(&installed.filename)?;
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

fn cleanup_unreferenced_rollback_artifacts(instance_mods_dir: &Path, snapshot: &RollbackSnapshot) {
    let files_dir = rollback_files_dir_path(instance_mods_dir);
    let keep: HashSet<&str> = snapshot
        .artifacts
        .iter()
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
