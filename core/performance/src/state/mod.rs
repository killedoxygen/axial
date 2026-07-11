use crate::MANAGED_ARTIFACT_MAX_BYTES;
use crate::types::{CompositionState, CompositionTier, OwnershipClass};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::warn;

const LOCK_FILE_NAME: &str = ".axial-lock.json";
const STATE_DIR_NAME: &str = ".axial-performance";
const ROLLBACK_DIR_NAME: &str = "rollback";
const ROLLBACK_FILE_NAME: &str = "latest.json";
const ROLLBACK_FILES_DIR_NAME: &str = "files";
const ROLLBACK_HISTORY_DIR_NAME: &str = "history";
const ROLLBACK_TMP_DIR_NAME: &str = "tmp";
const ROLLBACK_SCHEMA_VERSION: i32 = 1;
const ROLLBACK_HISTORY_LIMIT: usize = 5;
const ROLLBACK_METADATA_MAX_BYTES: u64 = 1024 * 1024;
const ROLLBACK_RETAINED_MAX_BYTES: u64 = MANAGED_ARTIFACT_MAX_BYTES * 2;
const ROLLBACK_TRANSIENT_MAX_BYTES: u64 =
    ROLLBACK_RETAINED_MAX_BYTES + MANAGED_ARTIFACT_MAX_BYTES + (ROLLBACK_METADATA_MAX_BYTES * 3);

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
    #[error("invalid performance artifact integrity for {filename}: {reason}")]
    InvalidIntegrity { filename: String, reason: String },
    #[error("invalid performance rollback snapshot id")]
    InvalidRollbackId,
    #[error("invalid rollback snapshot: {0}")]
    InvalidRollback(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackSnapshot {
    pub id: String,
    pub schema_version: i32,
    pub created_at: String,
    pub state: CompositionState,
    pub artifacts: Vec<RollbackArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSnapshotSummary {
    pub id: String,
    pub created_at: String,
    pub composition_id: String,
    pub tier: CompositionTier,
    pub installed_count: usize,
    pub artifact_count: usize,
    pub ownership_class: OwnershipClass,
    pub rollback_available: bool,
    pub latest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackArtifact {
    pub filename: String,
    pub stored_filename: String,
    pub project_id: String,
    pub version_id: String,
    pub ownership_class: OwnershipClass,
    pub sha512_present: bool,
    pub sha512_verified: bool,
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

pub async fn load_state_async(
    instance_mods_dir: &Path,
) -> Result<Option<CompositionState>, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    tokio::task::spawn_blocking(move || load_state(&instance_mods_dir))
        .await
        .map_err(|_| {
            StateError::Read(std::io::Error::other(
                "performance state load task stopped before reporting its result",
            ))
        })?
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
    let snapshot_id = new_rollback_snapshot_id();
    let planned = plan_rollback_artifacts(instance_mods_dir, state, &snapshot_id)?;
    finish_rollback_retention(instance_mods_dir)?;

    let snapshot = RollbackSnapshot {
        id: snapshot_id.clone(),
        schema_version: ROLLBACK_SCHEMA_VERSION,
        created_at: Utc::now().to_rfc3339(),
        state: state.clone(),
        artifacts: planned
            .artifacts
            .iter()
            .map(|artifact| artifact.metadata.clone())
            .collect(),
    };
    prepare_rollback_storage(
        instance_mods_dir,
        rollback_candidate_storage_bytes(&snapshot, planned.total_bytes)?,
    )?;
    commit_rollback_snapshot(instance_mods_dir, &planned, &snapshot)?;
    if let Err(_cleanup_error) = finish_rollback_retention(instance_mods_dir) {
        warn!("rollback retention cleanup remains pending after snapshot publication");
    }
    Ok(snapshot)
}

pub async fn save_rollback_snapshot_async(
    instance_mods_dir: &Path,
    state: &CompositionState,
) -> Result<RollbackSnapshot, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    let state = state.clone();
    tokio::task::spawn_blocking(move || save_rollback_snapshot(&instance_mods_dir, &state))
        .await
        .map_err(|_| {
            StateError::Read(std::io::Error::other(
                "rollback snapshot task stopped before reporting its result",
            ))
        })?
}

struct PlannedRollbackSnapshot {
    artifacts: Vec<PlannedRollbackArtifact>,
    total_bytes: u64,
}

struct PlannedRollbackArtifact {
    source_path: PathBuf,
    stored_path: PathBuf,
    expected_bytes: u64,
    metadata: RollbackArtifact,
}

fn plan_rollback_artifacts(
    instance_mods_dir: &Path,
    state: &CompositionState,
    snapshot_id: &str,
) -> Result<PlannedRollbackSnapshot, StateError> {
    let mut artifacts = Vec::new();
    let mut total_bytes = 0_u64;
    for (index, installed) in state.installed_mods.iter().enumerate() {
        let source_path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
        let source_metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(StateError::Read(error)),
        };
        if !source_metadata.file_type().is_file() {
            return Err(StateError::InvalidRollback(format!(
                "managed rollback source {} is not a regular file",
                installed.filename
            )));
        }
        let expected_bytes = source_metadata.len();
        validate_rollback_artifact_budget(&installed.filename, expected_bytes, &mut total_bytes)?;
        artifacts.push(planned_rollback_artifact(
            instance_mods_dir,
            snapshot_id,
            index,
            installed,
            source_path,
            expected_bytes,
        )?);
    }
    Ok(PlannedRollbackSnapshot {
        artifacts,
        total_bytes,
    })
}

fn validate_rollback_artifact_budget(
    filename: &str,
    artifact_bytes: u64,
    total_bytes: &mut u64,
) -> Result<(), StateError> {
    if artifact_bytes > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(StateError::InvalidRollback(format!(
            "rollback artifact {filename} exceeds the per-file byte budget"
        )));
    }
    *total_bytes = total_bytes.checked_add(artifact_bytes).ok_or_else(|| {
        StateError::InvalidRollback("rollback snapshot byte budget overflow".to_string())
    })?;
    if *total_bytes > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback snapshot exceeds the aggregate byte budget".to_string(),
        ));
    }
    Ok(())
}

fn planned_rollback_artifact(
    instance_mods_dir: &Path,
    snapshot_id: &str,
    index: usize,
    installed: &crate::types::InstalledMod,
    source_path: PathBuf,
    expected_bytes: u64,
) -> Result<PlannedRollbackArtifact, StateError> {
    let stored_filename = format!("{snapshot_id}-{index}.bin");
    validate_managed_filename(&stored_filename)?;
    Ok(PlannedRollbackArtifact {
        source_path,
        stored_path: rollback_files_dir_path(instance_mods_dir).join(&stored_filename),
        expected_bytes,
        metadata: RollbackArtifact {
            filename: installed.filename.clone(),
            stored_filename,
            project_id: installed.project_id.clone(),
            version_id: installed.version_id.clone(),
            ownership_class: installed.ownership_class,
            sha512_present: !installed.integrity.sha512.trim().is_empty(),
            sha512_verified: installed.integrity.sha512_verified,
        },
    })
}

fn prepare_rollback_storage(
    instance_mods_dir: &Path,
    candidate_bytes: u64,
) -> Result<(), StateError> {
    if candidate_bytes > ROLLBACK_RETAINED_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback candidate exceeds the retained byte budget".to_string(),
        ));
    }
    let existing_bytes = rollback_storage_bytes(instance_mods_dir)?;
    if !matches!(
        existing_bytes.checked_add(candidate_bytes),
        Some(bytes) if bytes <= ROLLBACK_TRANSIENT_MAX_BYTES
    ) {
        return Err(StateError::InvalidRollback(
            "rollback storage exceeds the total byte budget".to_string(),
        ));
    }
    Ok(())
}

fn rollback_candidate_storage_bytes(
    snapshot: &RollbackSnapshot,
    artifact_bytes: u64,
) -> Result<u64, StateError> {
    let metadata_bytes = u64::try_from(serde_json::to_vec_pretty(snapshot)?.len())
        .map_err(|_| StateError::InvalidRollback("rollback metadata size overflow".to_string()))?;
    if metadata_bytes > ROLLBACK_METADATA_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback metadata exceeds the byte budget".to_string(),
        ));
    }
    artifact_bytes
        .checked_add(metadata_bytes.saturating_mul(3))
        .ok_or_else(|| {
            StateError::InvalidRollback("rollback candidate byte budget overflow".to_string())
        })
}

fn rollback_storage_bytes(instance_mods_dir: &Path) -> Result<u64, StateError> {
    validate_rollback_internal_roots(instance_mods_dir)?;
    let mut total = 0_u64;
    let rollback_dir = rollback_dir_path(instance_mods_dir);
    if !rollback_dir.exists() {
        return Ok(0);
    }
    total_directory_files(&rollback_dir, false, &mut total)?;
    for path in [
        rollback_files_dir_path(instance_mods_dir),
        rollback_history_dir_path(instance_mods_dir),
        rollback_tmp_dir_path(instance_mods_dir),
    ] {
        total_directory_files(&path, true, &mut total)?;
    }
    Ok(total)
}

fn total_directory_files(
    directory: &Path,
    allow_no_subdirectories: bool,
    total: &mut u64,
) -> Result<(), StateError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file() {
            *total = total.checked_add(metadata.len()).ok_or_else(|| {
                StateError::InvalidRollback("rollback storage byte budget overflow".to_string())
            })?;
        } else if metadata.file_type().is_dir()
            && !allow_no_subdirectories
            && matches!(
                entry.file_name().to_str(),
                Some(ROLLBACK_FILES_DIR_NAME | ROLLBACK_HISTORY_DIR_NAME | ROLLBACK_TMP_DIR_NAME)
            )
        {
            continue;
        } else {
            return Err(StateError::InvalidRollback(
                "rollback storage contains a non-regular internal entry".to_string(),
            ));
        }
    }
    Ok(())
}

fn commit_rollback_snapshot(
    instance_mods_dir: &Path,
    planned: &PlannedRollbackSnapshot,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    ensure_rollback_internal_roots(instance_mods_dir)?;
    let mut copied_paths = Vec::new();
    for artifact in &planned.artifacts {
        if let Err(error) = copy_rollback_artifact(artifact) {
            cleanup_created_rollback_artifacts(&copied_paths)?;
            return Err(error);
        }
        copied_paths.push(artifact.stored_path.as_path());
    }
    let history_path = rollback_history_file_path(instance_mods_dir, &snapshot.id);
    let history_temp = match stage_new_rollback_snapshot(&history_path, snapshot) {
        Ok(temp_path) => temp_path,
        Err(error) => {
            cleanup_created_rollback_artifacts(&copied_paths)?;
            return Err(error);
        }
    };
    if let Err(error) = write_rollback_snapshot(&rollback_file_path(instance_mods_dir), snapshot) {
        let history_cleanup = remove_owned_file(&history_temp);
        let artifact_cleanup = cleanup_created_rollback_artifacts(&copied_paths);
        artifact_cleanup.or(history_cleanup)?;
        return Err(error);
    }
    if let Err(error) = fs::hard_link(&history_temp, history_path) {
        warn!("rollback history publication remains pending after latest publication");
        return Err(StateError::Read(error));
    }
    if let Err(_cleanup_error) = remove_owned_file(&history_temp) {
        warn!("rollback history temp cleanup remains pending after publication");
    }
    Ok(())
}

fn copy_rollback_artifact(artifact: &PlannedRollbackArtifact) -> Result<(), StateError> {
    let source_metadata = fs::symlink_metadata(&artifact.source_path)?;
    if !source_metadata.file_type().is_file() || source_metadata.len() != artifact.expected_bytes {
        return Err(StateError::InvalidRollback(format!(
            "managed rollback source {} changed before copy",
            artifact.metadata.filename
        )));
    }
    let source = fs::File::open(&artifact.source_path)?;
    let opened_metadata = source.metadata()?;
    if !opened_metadata.is_file() || opened_metadata.len() != artifact.expected_bytes {
        return Err(StateError::InvalidRollback(format!(
            "managed rollback source {} changed before copy",
            artifact.metadata.filename
        )));
    }
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&artifact.stored_path)?;
    let result = (|| {
        let copied = std::io::copy(
            &mut source.take(artifact.expected_bytes.saturating_add(1)),
            &mut destination,
        )?;
        destination.flush()?;
        Ok::<u64, std::io::Error>(copied)
    })();
    let copied = match result {
        Ok(copied) => copied,
        Err(error) => {
            drop(destination);
            remove_owned_file(&artifact.stored_path)?;
            return Err(StateError::Read(error));
        }
    };
    if copied != artifact.expected_bytes {
        drop(destination);
        remove_owned_file(&artifact.stored_path)?;
        return Err(StateError::InvalidRollback(format!(
            "managed rollback source {} changed during copy",
            artifact.metadata.filename
        )));
    }
    Ok(())
}

fn cleanup_created_rollback_artifacts(paths: &[&Path]) -> Result<(), StateError> {
    let mut first_error = None;
    for path in paths {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Some(error) = first_error {
        Err(StateError::Read(error))
    } else {
        Ok(())
    }
}

fn finish_rollback_retention(instance_mods_dir: &Path) -> Result<(), StateError> {
    reconcile_restore_stage_temps(instance_mods_dir)?;
    cleanup_proven_history_temps(instance_mods_dir)?;
    prune_rollback_history(instance_mods_dir)?;
    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    cleanup_unreferenced_rollback_artifacts(instance_mods_dir, &snapshots)?;
    cleanup_proven_latest_temp(instance_mods_dir)
}

pub fn load_rollback_snapshot(
    instance_mods_dir: &Path,
) -> Result<Option<RollbackSnapshot>, StateError> {
    validate_rollback_internal_roots(instance_mods_dir)?;
    let path = rollback_file_path(instance_mods_dir);
    let snapshot = match fs::symlink_metadata(&path) {
        Ok(_) => read_rollback_snapshot_file(&path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::Read(error)),
    };
    Ok(Some(snapshot))
}

pub async fn load_rollback_snapshot_async(
    instance_mods_dir: &Path,
) -> Result<Option<RollbackSnapshot>, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    tokio::task::spawn_blocking(move || load_rollback_snapshot(&instance_mods_dir))
        .await
        .map_err(|_| {
            StateError::Read(std::io::Error::other(
                "rollback load task stopped before reporting its result",
            ))
        })?
}

pub fn load_rollback_snapshot_by_id(
    instance_mods_dir: &Path,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    validate_rollback_internal_roots(instance_mods_dir)?;
    validate_rollback_snapshot_id(snapshot_id)?;
    let path = rollback_history_file_path(instance_mods_dir, snapshot_id);
    let snapshot = match fs::symlink_metadata(&path) {
        Ok(_) => read_rollback_snapshot_file(&path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::Read(error)),
    };
    if snapshot.id != snapshot_id {
        return Err(StateError::InvalidRollback(
            "snapshot id does not match history filename".to_string(),
        ));
    }
    Ok(Some(snapshot))
}

pub async fn load_rollback_snapshot_by_id_async(
    instance_mods_dir: &Path,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    let snapshot_id = snapshot_id.to_string();
    tokio::task::spawn_blocking(move || {
        load_rollback_snapshot_by_id(&instance_mods_dir, &snapshot_id)
    })
    .await
    .map_err(|_| {
        StateError::Read(std::io::Error::other(
            "rollback history load task stopped before reporting its result",
        ))
    })?
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
            artifact_count: record.snapshot.artifacts.len(),
            ownership_class: OwnershipClass::CompositionManaged,
            rollback_available: true,
            latest: record.latest,
        })
        .collect())
}

pub async fn list_rollback_snapshots_async(
    instance_mods_dir: &Path,
) -> Result<Vec<RollbackSnapshotSummary>, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    tokio::task::spawn_blocking(move || list_rollback_snapshots(&instance_mods_dir))
        .await
        .map_err(|_| {
            StateError::Read(std::io::Error::other(
                "rollback list task stopped before reporting its result",
            ))
        })?
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
    let current_state = load_state(instance_mods_dir)?;
    let current_filenames = managed_filenames(current_state.as_ref());

    let restore_targets =
        prepare_rollback_restore_targets(instance_mods_dir, snapshot, &current_filenames)?;
    stage_rollback_restore_targets(&restore_targets)?;
    let result = (|| {
        if let Some(current_state) = current_state {
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

        for target in &restore_targets {
            replace_file_atomic(&target.temp_path, &target.final_path)?;
        }

        save_state(instance_mods_dir, &snapshot.state)?;
        Ok(snapshot.state.clone())
    })();
    let cleanup = cleanup_rollback_restore_targets(&restore_targets);

    match (result, cleanup) {
        (Ok(state), Ok(())) => Ok(state),
        (Err(error), Ok(())) => Err(error),
        (_, Err(cleanup_error)) => Err(cleanup_error),
    }
}

pub async fn restore_rollback_snapshot_async(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<CompositionState, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || restore_rollback_snapshot(&instance_mods_dir, &snapshot))
        .await
        .map_err(|_| {
            StateError::Read(std::io::Error::other(
                "rollback restore task stopped before reporting its result",
            ))
        })?
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

fn rollback_tmp_dir_path(instance_mods_dir: &Path) -> PathBuf {
    rollback_dir_path(instance_mods_dir).join(ROLLBACK_TMP_DIR_NAME)
}

fn rollback_history_file_path(instance_mods_dir: &Path, snapshot_id: &str) -> PathBuf {
    rollback_history_dir_path(instance_mods_dir).join(format!("{snapshot_id}.json"))
}

fn validate_rollback_internal_roots(instance_mods_dir: &Path) -> Result<(), StateError> {
    let state_dir = instance_mods_dir.join(STATE_DIR_NAME);
    if !validate_existing_directory(&state_dir)? {
        return Ok(());
    }
    let rollback_dir = rollback_dir_path(instance_mods_dir);
    if !validate_existing_directory(&rollback_dir)? {
        return Ok(());
    }
    for path in [
        rollback_files_dir_path(instance_mods_dir),
        rollback_history_dir_path(instance_mods_dir),
        rollback_tmp_dir_path(instance_mods_dir),
    ] {
        validate_existing_directory(&path)?;
    }
    match fs::symlink_metadata(rollback_file_path(instance_mods_dir)) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(StateError::InvalidRollback(
                "latest rollback metadata is not a regular file".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(StateError::Read(error)),
    }
    Ok(())
}

fn ensure_rollback_internal_roots(instance_mods_dir: &Path) -> Result<(), StateError> {
    let state_dir = instance_mods_dir.join(STATE_DIR_NAME);
    for path in [
        state_dir,
        rollback_dir_path(instance_mods_dir),
        rollback_files_dir_path(instance_mods_dir),
        rollback_history_dir_path(instance_mods_dir),
        rollback_tmp_dir_path(instance_mods_dir),
    ] {
        ensure_managed_directory(&path)?;
    }
    Ok(())
}

fn validate_existing_directory(path: &Path) -> Result<bool, StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(StateError::InvalidRollback(
            "rollback internal path is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn ensure_managed_directory(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(StateError::InvalidRollback(
            "rollback internal path is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            if validate_existing_directory(path)? {
                Ok(())
            } else {
                Err(StateError::InvalidRollback(
                    "rollback internal directory was not created".to_string(),
                ))
            }
        }
        Err(error) => Err(StateError::Read(error)),
    }
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

    for artifact in &snapshot.artifacts {
        validate_managed_filename(&artifact.filename)?;
        validate_managed_filename(&artifact.stored_filename)?;
        let Some(installed) = snapshot
            .state
            .installed_mods
            .iter()
            .find(|installed| installed.filename == artifact.filename)
        else {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} is not in the rollback state",
                artifact.filename
            )));
        };
        if artifact.ownership_class != OwnershipClass::CompositionManaged
            || artifact.ownership_class != installed.ownership_class
        {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} has invalid rollback ownership",
                artifact.filename
            )));
        }
        if artifact.project_id != installed.project_id
            || artifact.version_id != installed.version_id
            || artifact.sha512_present == installed.integrity.sha512.trim().is_empty()
            || artifact.sha512_verified != installed.integrity.sha512_verified
        {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} metadata does not match rollback state",
                artifact.filename
            )));
        }
        if artifact.sha512_verified && !artifact.sha512_present {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} has invalid rollback integrity evidence",
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

fn managed_filenames(state: Option<&CompositionState>) -> HashSet<String> {
    state
        .map(|state| {
            state
                .installed_mods
                .iter()
                .map(|installed| installed.filename.clone())
                .collect()
        })
        .unwrap_or_default()
}

struct RollbackRestoreTarget {
    source_path: PathBuf,
    temp_path: PathBuf,
    final_path: PathBuf,
}

fn prepare_rollback_restore_targets(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
    current_filenames: &HashSet<String>,
) -> Result<Vec<RollbackRestoreTarget>, StateError> {
    reconcile_restore_stage_temps(instance_mods_dir)?;
    let files_dir = rollback_files_dir_path(instance_mods_dir);
    let mut targets = Vec::with_capacity(snapshot.artifacts.len());
    let mut total_bytes = 0_u64;
    let stage_id = new_rollback_restore_stage_id(snapshot);
    for (index, artifact) in snapshot.artifacts.iter().enumerate() {
        let source_path = files_dir.join(&artifact.stored_filename);
        let artifact_bytes =
            regular_rollback_artifact_bytes(&source_path, &artifact.stored_filename)?;
        validate_rollback_artifact_budget(&artifact.filename, artifact_bytes, &mut total_bytes)?;
        let final_path = managed_artifact_path(instance_mods_dir, &artifact.filename)?;
        if !current_filenames.contains(&artifact.filename) && path_exists(&final_path)? {
            return Err(StateError::InvalidRollback(format!(
                "rollback target {} is not tracked by current managed state",
                artifact.filename
            )));
        }
        targets.push(RollbackRestoreTarget {
            source_path,
            temp_path: rollback_restore_temp_path(instance_mods_dir, &stage_id, index),
            final_path,
        });
    }
    prepare_rollback_storage(instance_mods_dir, total_bytes)?;
    Ok(targets)
}

fn rollback_restore_temp_path(instance_mods_dir: &Path, stage_id: &str, index: usize) -> PathBuf {
    rollback_tmp_dir_path(instance_mods_dir).join(format!("{stage_id}-{index}-restore.tmp"))
}

fn new_rollback_restore_stage_id(snapshot: &RollbackSnapshot) -> String {
    format!(
        "restore--{}--{}--{}",
        snapshot.id,
        std::process::id(),
        Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| Utc::now().timestamp_millis())
    )
}

fn reconcile_restore_stage_temps(instance_mods_dir: &Path) -> Result<(), StateError> {
    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    let entries = match fs::read_dir(rollback_tmp_dir_path(instance_mods_dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if !metadata.is_file() {
            continue;
        }
        let Some(filename) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(stem) = filename.strip_suffix("-restore.tmp") else {
            continue;
        };
        let Some((prefix, raw_index)) = stem.rsplit_once('-') else {
            continue;
        };
        let Ok(index) = raw_index.parse::<usize>() else {
            continue;
        };
        let Some(snapshot) = snapshots.iter().find_map(|record| {
            let stage_prefix = format!("restore--{}--", record.snapshot.id);
            prefix
                .starts_with(&stage_prefix)
                .then_some(&record.snapshot)
        }) else {
            continue;
        };
        let Some(artifact) = snapshot.artifacts.get(index) else {
            continue;
        };
        let source = rollback_files_dir_path(instance_mods_dir).join(&artifact.stored_filename);
        if bounded_regular_files_match(&entry.path(), &source)? {
            remove_owned_file(&entry.path())?;
        }
    }
    Ok(())
}

fn bounded_regular_files_match(left: &Path, right: &Path) -> Result<bool, StateError> {
    let left_metadata = match fs::symlink_metadata(left) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    };
    let right_metadata = match fs::symlink_metadata(right) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    };
    if left_metadata.len() != right_metadata.len()
        || left_metadata.len() > MANAGED_ARTIFACT_MAX_BYTES
    {
        return Ok(false);
    }
    Ok(bounded_file_sha512(left, left_metadata.len())?
        == bounded_file_sha512(right, right_metadata.len())?)
}

fn bounded_file_sha512(path: &Path, expected_bytes: u64) -> Result<Vec<u8>, StateError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > expected_bytes || total > MANAGED_ARTIFACT_MAX_BYTES {
            return Ok(Vec::new());
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected_bytes {
        return Ok(Vec::new());
    }
    Ok(hasher.finalize().to_vec())
}

fn regular_rollback_artifact_bytes(path: &Path, stored_filename: &str) -> Result<u64, StateError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        StateError::InvalidRollback(format!("missing rollback artifact {stored_filename}"))
    })?;
    if metadata.file_type().is_file() {
        return Ok(metadata.len());
    }
    Err(StateError::InvalidRollback(format!(
        "rollback artifact {stored_filename} is not a regular file"
    )))
}

fn stage_rollback_restore_targets(targets: &[RollbackRestoreTarget]) -> Result<(), StateError> {
    if let Some(first) = targets.first() {
        let instance_mods_dir = first
            .final_path
            .parent()
            .ok_or_else(|| StateError::InvalidRollback("invalid rollback target".to_string()))?;
        ensure_rollback_internal_roots(instance_mods_dir)?;
    }
    let mut staged = Vec::new();
    for target in targets {
        let result = copy_regular_file_exclusive(&target.source_path, &target.temp_path);
        if let Err(error) = result {
            cleanup_created_rollback_artifacts(&staged)?;
            return Err(error);
        }
        staged.push(target.temp_path.as_path());
    }
    Ok(())
}

fn copy_regular_file_exclusive(source_path: &Path, target_path: &Path) -> Result<(), StateError> {
    let metadata = fs::symlink_metadata(source_path)?;
    if !metadata.file_type().is_file() || metadata.len() > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback source is not a bounded regular file".to_string(),
        ));
    }
    let expected_bytes = metadata.len();
    let source = fs::File::open(source_path)?;
    if source.metadata()?.len() != expected_bytes {
        return Err(StateError::InvalidRollback(
            "rollback source changed before staging".to_string(),
        ));
    }
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target_path)?;
    let result = std::io::copy(
        &mut source.take(expected_bytes.saturating_add(1)),
        &mut target,
    )
    .and_then(|copied| {
        target.flush()?;
        Ok(copied)
    });
    match result {
        Ok(copied) if copied == expected_bytes => Ok(()),
        Ok(_) => {
            drop(target);
            remove_owned_file(target_path)?;
            Err(StateError::InvalidRollback(
                "rollback source changed during staging".to_string(),
            ))
        }
        Err(error) => {
            drop(target);
            remove_owned_file(target_path)?;
            Err(StateError::Read(error))
        }
    }
}

fn remove_owned_file(path: &Path) -> Result<(), StateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn cleanup_rollback_restore_targets(targets: &[RollbackRestoreTarget]) -> Result<(), StateError> {
    for target in targets {
        match fs::remove_file(&target.temp_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StateError::Read(error)),
        }
    }
    Ok(())
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
        validate_sha512_integrity(&installed.filename, &installed.integrity.sha512)?;
        if installed.integrity.sha512_verified && installed.integrity.sha512.is_empty() {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "verified SHA-512 metadata requires a recorded SHA-512".to_string(),
            });
        }
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, StateError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn validate_sha512_integrity(filename: &str, sha512: &str) -> Result<(), StateError> {
    if sha512.is_empty() {
        return Ok(());
    }
    if sha512.trim() != sha512
        || sha512.len() != 128
        || !sha512.bytes().all(|value| value.is_ascii_hexdigit())
    {
        return Err(StateError::InvalidIntegrity {
            filename: filename.to_string(),
            reason: "SHA-512 metadata must be 128 hexadecimal characters".to_string(),
        });
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
) -> Result<(), StateError> {
    let files_dir = rollback_files_dir_path(instance_mods_dir);
    let keep: HashSet<&str> = snapshots
        .iter()
        .flat_map(|record| record.snapshot.artifacts.iter())
        .map(|artifact| artifact.stored_filename.as_str())
        .collect();
    let entries = match fs::read_dir(files_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            return Err(StateError::InvalidRollback(
                "rollback files contain a non-regular entry".to_string(),
            ));
        }
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        if !keep.contains(filename) && is_canonical_rollback_artifact_filename(filename) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn is_canonical_rollback_artifact_filename(filename: &str) -> bool {
    let Some(stem) = filename.strip_suffix(".bin") else {
        return false;
    };
    let Some((snapshot_id, index)) = stem.rsplit_once('-') else {
        return false;
    };
    index.parse::<usize>().is_ok() && validate_rollback_snapshot_id(snapshot_id).is_ok()
}

fn cleanup_proven_history_temps(instance_mods_dir: &Path) -> Result<(), StateError> {
    let history_dir = rollback_history_dir_path(instance_mods_dir);
    let entries = match fs::read_dir(&history_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        let Some(filename) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(snapshot_id) = filename.strip_suffix(".json.new.tmp") else {
            continue;
        };
        validate_rollback_snapshot_id(snapshot_id)?;
        let temp_path = entry.path();
        let final_path = rollback_history_file_path(instance_mods_dir, snapshot_id);
        let temp = read_bounded_regular_metadata_file(&temp_path)?;
        let snapshot = serde_json::from_slice::<RollbackSnapshot>(&temp)?;
        validate_rollback_snapshot(&snapshot)?;
        if snapshot.id != snapshot_id {
            return Err(StateError::InvalidRollback(
                "rollback history temp id does not match its filename".to_string(),
            ));
        }
        match fs::symlink_metadata(&final_path) {
            Ok(_) if temp == read_bounded_regular_metadata_file(&final_path)? => {
                remove_owned_file(&temp_path)?;
                continue;
            }
            Ok(_) => {
                return Err(StateError::InvalidRollback(
                    "rollback history temp conflicts with published history".to_string(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StateError::Read(error)),
        }
        let matches_latest = match fs::symlink_metadata(rollback_file_path(instance_mods_dir)) {
            Ok(_) => {
                temp == read_bounded_regular_metadata_file(&rollback_file_path(instance_mods_dir))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(StateError::Read(error)),
        };
        if matches_latest {
            fs::hard_link(&temp_path, &final_path)?;
        } else {
            cleanup_abandoned_snapshot_artifacts(instance_mods_dir, &snapshot)?;
        }
        remove_owned_file(&temp_path)?;
    }
    Ok(())
}

fn cleanup_abandoned_snapshot_artifacts(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    for (index, artifact) in snapshot.artifacts.iter().enumerate() {
        if artifact.stored_filename != format!("{}-{index}.bin", snapshot.id) {
            return Err(StateError::InvalidRollback(
                "rollback candidate artifact identity is invalid".to_string(),
            ));
        }
        remove_owned_file(
            &rollback_files_dir_path(instance_mods_dir).join(&artifact.stored_filename),
        )?;
    }
    Ok(())
}

fn cleanup_proven_latest_temp(instance_mods_dir: &Path) -> Result<(), StateError> {
    let temp_path = rollback_file_path(instance_mods_dir).with_extension("json.tmp");
    let temp_data = match fs::symlink_metadata(&temp_path) {
        Ok(_) => read_bounded_regular_metadata_file(&temp_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let temp_snapshot = serde_json::from_slice::<RollbackSnapshot>(&temp_data)?;
    validate_rollback_snapshot(&temp_snapshot)?;
    let matches_latest = match fs::symlink_metadata(rollback_file_path(instance_mods_dir)) {
        Ok(_) => {
            temp_data == read_bounded_regular_metadata_file(&rollback_file_path(instance_mods_dir))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(StateError::Read(error)),
    };
    let candidate_artifacts_absent = temp_snapshot.artifacts.iter().all(|artifact| {
        !rollback_files_dir_path(instance_mods_dir)
            .join(&artifact.stored_filename)
            .exists()
    });
    if !matches_latest && !candidate_artifacts_absent {
        return Err(StateError::InvalidRollback(
            "latest rollback temp ownership cannot be proven".to_string(),
        ));
    }
    remove_owned_file(&temp_path)
}

fn read_bounded_regular_metadata_file(path: &Path) -> Result<Vec<u8>, StateError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > ROLLBACK_METADATA_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback metadata obligation is not a bounded regular file".to_string(),
        ));
    }
    let mut file = fs::File::open(path)?;
    let opened = file.metadata()?;
    let after = fs::symlink_metadata(path)?;
    if !opened.is_file()
        || !after.file_type().is_file()
        || !same_file_identity(&opened, &after)
        || opened.len() != metadata.len()
        || after.len() != metadata.len()
    {
        return Err(StateError::InvalidRollback(
            "rollback metadata changed while opening".to_string(),
        ));
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(ROLLBACK_METADATA_MAX_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 != metadata.len() {
        return Err(StateError::InvalidRollback(
            "rollback metadata changed while reconciling cleanup".to_string(),
        ));
    }
    Ok(data)
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn read_rollback_snapshot_file(path: &Path) -> Result<RollbackSnapshot, StateError> {
    let data = read_bounded_regular_metadata_file(path)?;
    let snapshot = serde_json::from_slice::<RollbackSnapshot>(&data)?;
    validate_rollback_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn write_rollback_snapshot(path: &Path, snapshot: &RollbackSnapshot) -> Result<(), StateError> {
    let data = serde_json::to_string_pretty(snapshot)?;
    let temp_path = path.with_extension("json.tmp");
    let mut temp = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    if let Err(error) = temp.write_all(data.as_bytes()).and_then(|()| temp.flush()) {
        drop(temp);
        remove_owned_file(&temp_path)?;
        return Err(StateError::Read(error));
    }
    drop(temp);
    replace_file_atomic(&temp_path, path)?;
    Ok(())
}

fn stage_new_rollback_snapshot(
    path: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<PathBuf, StateError> {
    let data = serde_json::to_string_pretty(snapshot)?;
    let temp_path = path.with_extension("json.new.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    if let Err(error) = file.write_all(data.as_bytes()).and_then(|()| file.flush()) {
        drop(file);
        remove_owned_file(&temp_path)?;
        return Err(StateError::Read(error));
    }
    drop(file);
    Ok(temp_path)
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
            return Err(StateError::InvalidRollback(
                "rollback history contains a non-regular entry".to_string(),
            ));
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(snapshot_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        validate_rollback_snapshot_id(snapshot_id)?;
        let snapshot = read_rollback_snapshot_file(&path)?;
        if snapshot.id != snapshot_id {
            return Err(StateError::InvalidRollback(
                "snapshot id does not match history filename".to_string(),
            ));
        }
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
    let mut keep = HashSet::new();
    let mut retained_bytes = 0_u64;
    if let Some(latest) = snapshots.iter().find(|record| record.latest) {
        let latest_bytes = rollback_snapshot_record_bytes(instance_mods_dir, latest)?;
        if latest_bytes > ROLLBACK_RETAINED_MAX_BYTES {
            return Err(StateError::InvalidRollback(
                "latest rollback snapshot exceeds the retained byte budget".to_string(),
            ));
        }
        retained_bytes = latest_bytes;
        keep.insert(latest.snapshot.id.clone());
    }
    for record in snapshots.iter().filter(|record| !record.latest) {
        let record_bytes = rollback_snapshot_record_bytes(instance_mods_dir, record)?;
        let admitted = retained_bytes
            .checked_add(record_bytes)
            .is_some_and(|bytes| bytes <= ROLLBACK_RETAINED_MAX_BYTES);
        if admitted && keep.len() < ROLLBACK_HISTORY_LIMIT {
            retained_bytes += record_bytes;
            keep.insert(record.snapshot.id.clone());
        }
    }
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
            return Err(StateError::InvalidRollback(
                "rollback history contains a non-regular entry".to_string(),
            ));
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

fn rollback_snapshot_record_bytes(
    instance_mods_dir: &Path,
    record: &RollbackSnapshotRecord,
) -> Result<u64, StateError> {
    let mut total = 0_u64;
    let mut seen = HashSet::new();
    for artifact in &record.snapshot.artifacts {
        if seen.insert(artifact.stored_filename.as_str()) {
            let bytes = regular_rollback_artifact_bytes(
                &rollback_files_dir_path(instance_mods_dir).join(&artifact.stored_filename),
                &artifact.stored_filename,
            )?;
            total = total.checked_add(bytes).ok_or_else(|| {
                StateError::InvalidRollback("rollback retained byte budget overflow".to_string())
            })?;
        }
    }
    let history_path = rollback_history_file_path(instance_mods_dir, &record.snapshot.id);
    match fs::symlink_metadata(history_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            total = total.checked_add(metadata.len()).ok_or_else(|| {
                StateError::InvalidRollback("rollback retained byte budget overflow".to_string())
            })?;
        }
        Ok(_) => {
            return Err(StateError::InvalidRollback(
                "rollback history metadata is not a regular file".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(StateError::Read(error)),
    }
    if record.latest {
        let metadata = fs::symlink_metadata(rollback_file_path(instance_mods_dir))?;
        if !metadata.file_type().is_file() {
            return Err(StateError::InvalidRollback(
                "latest rollback metadata is not a regular file".to_string(),
            ));
        }
        total = total.checked_add(metadata.len()).ok_or_else(|| {
            StateError::InvalidRollback("rollback retained byte budget overflow".to_string())
        })?;
    }
    Ok(total)
}

fn replace_file_atomic(temp_path: &Path, final_path: &Path) -> Result<(), std::io::Error> {
    if fs::rename(temp_path, final_path).is_ok() {
        return Ok(());
    }

    if final_path.exists() {
        fs::remove_file(final_path)?;
    }

    match fs::rename(temp_path, final_path) {
        Ok(()) => Ok(()),
        Err(error) => match fs::remove_file(temp_path) {
            Ok(()) => Err(error),
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => {
                Err(error)
            }
            Err(cleanup_error) => Err(cleanup_error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        InstalledMod, ManagedArtifactIntegrity, ManagedArtifactProvider, ManagedArtifactSource,
    };

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
        assert!(summaries.iter().all(|summary| {
            summary.ownership_class == OwnershipClass::CompositionManaged
                && summary.rollback_available
                && summary.artifact_count == summary.installed_count
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_rollback_snapshot_is_still_available_evidence() {
        let root = test_root("empty-rollback-evidence");
        let snapshot = save_rollback_snapshot(&root, &test_state("vanilla-enhanced", Vec::new()))
            .expect("save empty rollback snapshot");

        let summaries = list_rollback_snapshots(&root).expect("list rollback snapshots");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, snapshot.id);
        assert_eq!(summaries[0].installed_count, 0);
        assert_eq!(summaries[0].artifact_count, 0);
        assert!(summaries[0].rollback_available);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_rejects_missing_artifact_metadata() {
        let root = test_root("missing-rollback-artifact-metadata");
        fs::create_dir_all(rollback_files_dir_path(&root)).expect("create rollback files dir");
        fs::write(
            rollback_files_dir_path(&root).join("missing-metadata.bin"),
            b"managed-a",
        )
        .expect("write rollback artifact");
        fs::write(
            rollback_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "id": "rb-missing-metadata",
                "schema_version": ROLLBACK_SCHEMA_VERSION,
                "created_at": "2026-05-30T00:00:00Z",
                "state": test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
                "artifacts": [{
                    "filename": "managed-a.jar",
                    "stored_filename": "missing-metadata.bin"
                }]
            }))
            .expect("serialize rollback snapshot"),
        )
        .expect("write rollback snapshot");

        assert!(matches!(
            load_rollback_snapshot(&root)
                .expect_err("missing rollback artifact metadata should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_rollback_snapshot_saves_managed_artifacts() {
        let root = test_root("async-save-rollback");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed");
        fs::write(root.join("user.jar"), b"user").expect("write user");

        let snapshot = save_rollback_snapshot_async(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .await
        .expect("save rollback snapshot async");

        assert_eq!(snapshot.artifacts.len(), 1);
        let artifact = &snapshot.artifacts[0];
        assert_eq!(artifact.filename, "managed-a.jar");
        assert_eq!(artifact.project_id, "sodium");
        assert_eq!(artifact.version_id, "version");
        assert_eq!(artifact.ownership_class, OwnershipClass::CompositionManaged);
        assert!(!artifact.sha512_present);
        assert!(!artifact.sha512_verified);
        assert_eq!(
            fs::read(rollback_files_dir_path(&root).join(&artifact.stored_filename))
                .expect("read stored artifact"),
            b"managed-a"
        );
        let latest = load_rollback_snapshot(&root)
            .expect("load latest")
            .expect("latest snapshot");
        assert_eq!(latest.id, snapshot.id);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_rejects_oversized_artifact_before_copy() {
        let root = test_root("snapshot-artifact-byte-budget");
        let source_path = root.join("managed-a.jar");
        fs::File::create(&source_path)
            .expect("create sparse managed artifact")
            .set_len(MANAGED_ARTIFACT_MAX_BYTES + 1)
            .expect("size sparse managed artifact");

        let error = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect_err("oversized rollback artifact should fail before copy");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert!(!rollback_dir_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_rejects_aggregate_budget_before_copy() {
        let root = test_root("snapshot-aggregate-byte-budget");
        for filename in ["managed-a.jar", "managed-b.jar"] {
            fs::File::create(root.join(filename))
                .expect("create sparse managed artifact")
                .set_len((MANAGED_ARTIFACT_MAX_BYTES / 2) + 1)
                .expect("size sparse managed artifact");
        }

        let error = save_rollback_snapshot(
            &root,
            &test_state(
                "core-a",
                vec![
                    test_mod("sodium", "managed-a.jar"),
                    test_mod("lithium", "managed-b.jar"),
                ],
            ),
        )
        .expect_err("aggregate rollback budget should fail before copy");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert!(!rollback_dir_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_accounts_for_transient_candidate_storage() {
        let root = test_root("snapshot-total-storage-budget");
        let files_dir = rollback_files_dir_path(&root);
        fs::create_dir_all(&files_dir).expect("create rollback files dir");
        fs::File::create(files_dir.join("retained.bin"))
            .expect("create retained sparse artifact")
            .set_len(ROLLBACK_TRANSIENT_MAX_BYTES)
            .expect("size retained sparse artifact");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");

        let error = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect_err("transient successor must fit beside retained storage");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(fs::read_dir(&files_dir).expect("read files").count(), 1);
        assert!(!rollback_file_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_storage_budget_counts_internal_temp_and_metadata_files() {
        let root = test_root("snapshot-internal-storage-budget");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        fs::File::create(rollback_tmp_dir_path(&root).join("pending.tmp"))
            .expect("create pending temp")
            .set_len(ROLLBACK_TRANSIENT_MAX_BYTES)
            .expect("size pending temp");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");

        let error = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect_err("internal temp bytes must count against transient storage");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert!(!rollback_file_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_retries_orphan_cleanup_before_successor() {
        let root = test_root("snapshot-orphan-cleanup-retry");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let orphan = rollback_files_dir_path(&root).join("rb-orphan-0.bin");
        fs::write(&orphan, b"partial-candidate").expect("write orphan candidate");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");

        save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("successor should settle orphan cleanup first");

        assert!(!orphan.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rollback_snapshot_rejects_symlinked_internal_root() {
        use std::os::unix::fs::symlink;

        let root = test_root("snapshot-symlink-internal-root");
        let external = test_root("snapshot-symlink-internal-external");
        symlink(&external, root.join(STATE_DIR_NAME)).expect("symlink internal state root");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");

        let error = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect_err("symlinked internal root must be rejected");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(fs::read_dir(&external).expect("read external").count(), 0);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(external);
    }

    #[cfg(unix)]
    #[test]
    fn rollback_snapshot_rejects_symlink_managed_source() {
        use std::os::unix::fs::symlink;

        let root = test_root("snapshot-symlink-source");
        let victim = root.join("victim.jar");
        fs::write(&victim, b"user-owned").expect("write symlink victim");
        symlink(&victim, root.join("managed-a.jar")).expect("create managed source symlink");

        let error = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect_err("symlink managed source must not be copied");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(fs::read(victim).expect("read victim"), b"user-owned");
        assert!(!rollback_dir_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_cleans_candidate_when_latest_metadata_cannot_publish() {
        let root = test_root("snapshot-metadata-failure-cleanup");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let retained = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save retained snapshot");
        fs::create_dir(rollback_file_path(&root).with_extension("json.tmp"))
            .expect("block latest metadata temp");
        fs::write(root.join("managed-b.jar"), b"managed-b").expect("write managed b");

        save_rollback_snapshot(
            &root,
            &test_state("core-b", vec![test_mod("lithium", "managed-b.jar")]),
        )
        .expect_err("blocked latest metadata should reject candidate");

        let latest = load_rollback_snapshot(&root)
            .expect("load retained latest")
            .expect("retained latest exists");
        assert_eq!(latest.id, retained.id);
        assert_eq!(
            fs::read_dir(rollback_files_dir_path(&root))
                .expect("read rollback files")
                .count(),
            1,
            "failed candidate artifact should be removed"
        );
        assert_eq!(
            fs::read_dir(rollback_history_dir_path(&root))
                .expect("read rollback history")
                .count(),
            1,
            "failed candidate history should be removed"
        );
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

        fs::remove_file(root.join("managed-a.jar")).expect("remove superseded managed a");
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
    fn rollback_refuses_to_claim_untracked_matching_target() {
        let root = test_root("restore-untracked-matching-target");
        fs::write(root.join("managed-a.jar"), b"snapshot-managed").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");

        let error = restore_rollback_snapshot(&root, &snapshot)
            .expect_err("matching bytes are not ownership proof");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read target"),
            b"snapshot-managed"
        );
        assert!(load_state(&root).expect("load state").is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_refuses_to_overwrite_untracked_existing_target() {
        let root = test_root("restore-untracked-existing-target");
        fs::write(root.join("managed-a.jar"), b"snapshot-managed").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("replace target");

        let error = restore_rollback_snapshot(&root, &snapshot)
            .expect_err("rollback must not overwrite untracked target");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read target"),
            b"user-replacement"
        );
        assert!(
            load_state(&root).expect("load state").is_none(),
            "rollback should not write state after refusing overwrite"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_rejects_corrupt_current_state_before_mutation() {
        let root = test_root("restore-corrupt-current-state");
        fs::write(root.join("managed-a.jar"), b"snapshot-managed").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("replace target");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core-current",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "managed-a.jar",
                    "ownership_class": "user_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize current state"),
        )
        .expect("write corrupt current state");

        let error = restore_rollback_snapshot(&root, &snapshot)
            .expect_err("corrupt current ownership must block rollback");

        assert!(matches!(error, StateError::InvalidOwnership { .. }));
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read target"),
            b"user-replacement"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_validates_all_snapshot_artifacts_before_deleting_current_managed_files() {
        let root = test_root("restore-missing-artifact-preflight");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::remove_file(root.join("managed-a.jar")).expect("remove old managed a");
        fs::write(root.join("managed-b.jar"), b"managed-b").expect("write managed b");
        save_state(
            &root,
            &test_state("core-b", vec![test_mod("lithium", "managed-b.jar")]),
        )
        .expect("save current state");
        let artifact = snapshot.artifacts.first().expect("snapshot artifact");
        fs::remove_file(rollback_files_dir_path(&root).join(&artifact.stored_filename))
            .expect("remove snapshot artifact");

        let error = restore_rollback_snapshot(&root, &snapshot)
            .expect_err("missing snapshot artifact should fail before deletion");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(
            fs::read(root.join("managed-b.jar")).expect("read current managed"),
            b"managed-b"
        );
        assert_eq!(
            load_state(&root)
                .expect("load state")
                .expect("current state remains")
                .composition_id,
            "core-b"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_does_not_touch_user_owned_rollback_tmp_collision() {
        let root = test_root("restore-temp-collision");
        fs::write(root.join("managed-a.jar"), b"snapshot-managed").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::remove_file(root.join("managed-a.jar")).expect("remove target");
        let user_temp_path = root.join("managed-a.rollback.tmp");
        fs::write(&user_temp_path, b"user-temp").expect("write user temp");

        let restored =
            restore_rollback_snapshot(&root, &snapshot).expect("restore should use managed temp");

        assert_eq!(restored.composition_id, "core-a");
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read restored"),
            b"snapshot-managed"
        );
        assert_eq!(
            fs::read(user_temp_path).expect("read user temp"),
            b"user-temp"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_rollback_snapshot_restores_tracked_state_only() {
        let root = test_root("async-restore");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        fs::write(root.join("user.jar"), b"user-v1").expect("write user");
        let snapshot = save_rollback_snapshot_async(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .await
        .expect("save snapshot");

        fs::remove_file(root.join("managed-a.jar")).expect("remove superseded managed a");
        fs::write(root.join("managed-b.jar"), b"managed-b").expect("write managed b");
        save_state(
            &root,
            &test_state("core-b", vec![test_mod("lithium", "managed-b.jar")]),
        )
        .expect("save current state");
        fs::write(root.join("user.jar"), b"user-v2").expect("mutate user");

        let restored = restore_rollback_snapshot_async(&root, &snapshot)
            .await
            .expect("restore async");

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

    #[tokio::test]
    async fn async_rollback_refuses_to_overwrite_untracked_existing_target() {
        let root = test_root("async-restore-untracked-existing-target");
        fs::write(root.join("managed-a.jar"), b"snapshot-managed").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("replace target");

        let error = restore_rollback_snapshot_async(&root, &snapshot)
            .await
            .expect_err("async rollback must not overwrite untracked target");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read target"),
            b"user-replacement"
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
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
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
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
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
    fn load_state_rejects_missing_or_unknown_source_and_integrity_shape() {
        let root = test_root("invalid-source-integrity-shape");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "composition_managed"
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize state"),
        )
        .expect("write missing source state");
        assert!(matches!(
            load_state(&root).expect_err("missing source and integrity should be invalid"),
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
                    "ownership_class": "composition_managed",
                    "source": { "provider": "unknown" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize state"),
        )
        .expect("write unknown source state");
        assert!(matches!(
            load_state(&root).expect_err("unknown source should be invalid"),
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
                    "ownership_class": "composition_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false, "path": "/tmp/sodium.jar" }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize state"),
        )
        .expect("write unknown integrity field state");
        assert!(matches!(
            load_state(&root).expect_err("unknown integrity field should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_missing_failure_metadata() {
        let root = test_root("missing-failure-metadata");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [],
                "installed_at": "2026-05-30T00:00:00Z"
            }))
            .expect("serialize state"),
        )
        .expect("write state without failure metadata");

        assert!(matches!(
            load_state(&root).expect_err("missing failure metadata should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_unknown_top_level_fields() {
        let root = test_root("unknown-top-level-state");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": "",
                "unexpected_state": true
            }))
            .expect("serialize state"),
        )
        .expect("write state with unknown field");

        assert!(matches!(
            load_state(&root).expect_err("unknown top-level state should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_rejects_missing_artifact_list() {
        let root = test_root("missing-rollback-artifacts");
        let rollback_dir = rollback_dir_path(&root);
        fs::create_dir_all(&rollback_dir).expect("create rollback dir");
        fs::write(
            rollback_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "id": "rb-missing-artifacts",
                "schema_version": ROLLBACK_SCHEMA_VERSION,
                "created_at": "2026-05-30T00:00:00Z",
                "state": test_state("core", Vec::new())
            }))
            .expect("serialize rollback snapshot"),
        )
        .expect("write rollback snapshot without artifacts");

        assert!(matches!(
            load_rollback_snapshot(&root).expect_err("missing artifacts should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_verified_integrity_without_sha512() {
        let root = test_root("invalid-integrity");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "composition_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": true }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize state"),
        )
        .expect("write invalid integrity state");

        let error = load_state(&root).expect_err("empty verified SHA-512 should be invalid");

        assert!(matches!(error, StateError::InvalidIntegrity { .. }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_malformed_sha512_metadata() {
        let root = test_root("malformed-sha512");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "composition_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "abc123", "sha512_verified": true }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize state"),
        )
        .expect("write malformed integrity state");

        let error = load_state(&root).expect_err("short SHA-512 should be invalid");

        assert!(matches!(error, StateError::InvalidIntegrity { .. }));
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
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
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
            source: ManagedArtifactSource {
                provider: ManagedArtifactProvider::Modrinth,
            },
            integrity: ManagedArtifactIntegrity {
                sha512: String::new(),
                sha512_verified: false,
            },
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-performance-state-{name}-{}-{}",
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
