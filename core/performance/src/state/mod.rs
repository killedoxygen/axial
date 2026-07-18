use crate::MANAGED_ARTIFACT_MAX_BYTES;
use crate::types::{CompositionState, CompositionTier, InstalledMod, OwnershipClass};
use axial_minecraft::managed_path::AnchoredDirectory;
use chrono::Utc;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const LOCK_FILE_NAME: &str = ".axial-lock.json";
const LOCK_STAGED_FILE_NAME: &str = ".axial-lock.json.new.tmp";
const LOCK_BACKUP_FILE_NAME: &str = ".axial-lock.json.previous.tmp";
const LOCK_DELETE_FILE_NAME: &str = ".axial-lock.json.delete.tmp";
const LOCK_DELETE_MARKER: &[u8] = b"axial-performance-state-delete-v1\n";
const STATE_SCHEMA_VERSION: i32 = 2;
const STATE_MAX_BYTES: u64 = 1024 * 1024;
const STATE_MAX_INSTALLED_MODS: usize = 256;
const STATE_TOKEN_MAX_CHARS: usize = 256;
const STATE_TIMESTAMP_MAX_CHARS: usize = 64;
const STATE_FILENAME_MAX_BYTES: usize = 255;
pub(crate) const STATE_DIR_NAME: &str = ".axial-performance";
const MUTATION_DIR_NAME: &str = "mutations";
const REMOVAL_DIR_NAME: &str = "removals";
const ADDITION_DIR_NAME: &str = "additions";
const QUARANTINE_DIR_NAME: &str = "quarantine";
const ROLLBACK_DIR_NAME: &str = "rollback";
const ROLLBACK_FILE_NAME: &str = "latest.json";
const ROLLBACK_FILES_DIR_NAME: &str = "files";
const ROLLBACK_HISTORY_DIR_NAME: &str = "history";
const ROLLBACK_TMP_DIR_NAME: &str = "tmp";
const ROLLBACK_SCHEMA_VERSION: i32 = 3;
const ROLLBACK_HISTORY_LIMIT: usize = 5;
const ROLLBACK_METADATA_MAX_BYTES: u64 = 1024 * 1024;
const ROLLBACK_RETAINED_MAX_BYTES: u64 = MANAGED_ARTIFACT_MAX_BYTES * 2;
const ROLLBACK_TRANSIENT_MAX_BYTES: u64 =
    ROLLBACK_RETAINED_MAX_BYTES + MANAGED_ARTIFACT_MAX_BYTES + (ROLLBACK_METADATA_MAX_BYTES * 3);
pub(crate) const RECOVERY_ENTRY_LIMIT: usize = 1024;
const CLEANUP_QUARANTINE_MAX_BYTES: u64 = MANAGED_ARTIFACT_MAX_BYTES * 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RollbackDurabilityPoint {
    DirectoryCreated,
    ArtifactPublished,
    ArtifactCopiesPublished,
    ArtifactCopiesCleaned,
    HistoryStaged,
    HistoryCandidateCleaned,
    HistoryPublished,
    LatestStaged,
    LatestBackedUp,
    LatestPublished,
    LatestRestored,
}

#[cfg(test)]
thread_local! {
    static ROLLBACK_DURABILITY_FAILURE: std::cell::Cell<Option<RollbackDurabilityPoint>> =
        const { std::cell::Cell::new(None) };
}

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
    #[error("invalid performance state: {0}")]
    InvalidState(String),
    #[error("performance state publication failed during {phase}: {source}")]
    Publication {
        phase: StatePublicationPhase,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatePublicationPhase {
    Reconcile,
    Stage,
    Backup,
    Publish,
    Cleanup,
}

impl std::fmt::Display for StatePublicationPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconcile => "reconciliation",
            Self::Stage => "staging",
            Self::Backup => "backup",
            Self::Publish => "publication",
            Self::Cleanup => "cleanup",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCompositionState {
    schema_version: i32,
    state: CompositionState,
}

struct AdmittedPersistedCompositionState {
    snapshot: PersistedCompositionState,
    sha512: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollbackSnapshot {
    pub id: String,
    pub schema_version: i32,
    pub created_at: String,
    pub target: RollbackSnapshotState,
    pub artifacts: Vec<RollbackArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RollbackSnapshotState {
    ManagedStateAbsent,
    ManagedComposition { state: CompositionState },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackSnapshotTarget {
    ManagedStateAbsent,
    ManagedComposition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedRollbackOutcome {
    ManagedStateAbsent,
    ManagedComposition(CompositionState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSnapshotSummary {
    pub id: String,
    pub created_at: String,
    pub target: RollbackSnapshotTarget,
    pub composition_id: Option<String>,
    pub tier: Option<CompositionTier>,
    pub installed_count: usize,
    pub artifact_count: usize,
    pub ownership_class: OwnershipClass,
    pub rollback_available: bool,
    pub latest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollbackArtifact {
    pub filename: String,
    pub stored_filename: String,
    pub project_id: String,
    pub version_id: String,
    pub ownership_class: OwnershipClass,
    pub sha512: String,
}

impl RollbackSnapshot {
    fn state(&self) -> Option<&CompositionState> {
        match &self.target {
            RollbackSnapshotState::ManagedStateAbsent => None,
            RollbackSnapshotState::ManagedComposition { state } => Some(state),
        }
    }

    fn target_kind(&self) -> RollbackSnapshotTarget {
        match &self.target {
            RollbackSnapshotState::ManagedStateAbsent => RollbackSnapshotTarget::ManagedStateAbsent,
            RollbackSnapshotState::ManagedComposition { .. } => {
                RollbackSnapshotTarget::ManagedComposition
            }
        }
    }
}

pub(crate) fn load_state(instance_mods_dir: &Path) -> Result<Option<CompositionState>, StateError> {
    reconcile_managed_storage(instance_mods_dir)?;
    load_state_admitted(instance_mods_dir)
}

pub(crate) fn reconcile_managed_storage(instance_mods_dir: &Path) -> Result<(), StateError> {
    reconcile_cleanup_quarantine(instance_mods_dir)?;
    reconcile_state_publication(instance_mods_dir)?;
    let state = load_state_admitted(instance_mods_dir)?;
    reconcile_managed_addition_obligations(instance_mods_dir, state.as_ref())?;
    reconcile_managed_removal_obligations(instance_mods_dir, state.as_ref())?;
    reconcile_rollback_metadata(instance_mods_dir)
}

pub(crate) fn recover_managed_storage(
    instance_mods_dir: &Path,
) -> Result<Option<CompositionState>, StateError> {
    reconcile_managed_storage(instance_mods_dir)?;
    finish_rollback_retention(instance_mods_dir)?;
    reconcile_managed_storage(instance_mods_dir)?;
    load_state_admitted(instance_mods_dir)
}

pub(crate) fn prove_managed_storage_recovered(
    instance_mods_dir: &Path,
    state: Option<&CompositionState>,
) -> Result<(), StateError> {
    for path in [
        state_staged_path(instance_mods_dir),
        state_backup_path(instance_mods_dir),
        state_delete_path(instance_mods_dir),
    ] {
        if path_exists(&path)? {
            return Err(StateError::InvalidState(
                "managed state publication obligation remains after recovery".to_string(),
            ));
        }
    }
    prove_managed_internal_roots(instance_mods_dir)?;
    prove_removal_obligations_settled(instance_mods_dir)?;
    prove_rollback_storage_settled(instance_mods_dir)?;
    for installed in state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        if !managed_artifact_matches(instance_mods_dir, installed)? {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason:
                    "tracked managed artifact is missing or does not match its ownership digest"
                        .to_string(),
            });
        }
    }
    Ok(())
}

pub(crate) fn load_state_admitted(
    instance_mods_dir: &Path,
) -> Result<Option<CompositionState>, StateError> {
    Ok(
        read_state_snapshot_if_present(&lock_file_path(instance_mods_dir))?
            .map(|snapshot| snapshot.snapshot.state),
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedInspectionReconciliation {
    state_publication: bool,
    managed_addition: bool,
    managed_removal: bool,
    rollback_publication: bool,
    cleanup_quarantine: bool,
}

impl ManagedInspectionReconciliation {
    pub(crate) const fn state_publication_required(self) -> bool {
        self.state_publication
    }

    pub(crate) const fn admitted_state_reconciliation_required(self) -> bool {
        self.managed_addition
            || self.managed_removal
            || self.rollback_publication
            || self.cleanup_quarantine
    }
}

pub(crate) fn preflight_managed_inspection_reconciliation(
    instance_mods_dir: &Path,
) -> Result<ManagedInspectionReconciliation, StateError> {
    Ok(ManagedInspectionReconciliation {
        state_publication: state_publication_reconciliation_required(instance_mods_dir)?,
        managed_addition: managed_addition_reconciliation_required(instance_mods_dir)?,
        managed_removal: managed_removal_reconciliation_required(instance_mods_dir)?,
        rollback_publication: rollback_publication_reconciliation_required(instance_mods_dir)?,
        cleanup_quarantine: cleanup_quarantine_reconciliation_required(instance_mods_dir)?,
    })
}

pub(crate) fn reconcile_managed_inspection_publication(
    instance_mods_dir: &Path,
    preflight: ManagedInspectionReconciliation,
) -> Result<(), StateError> {
    if preflight.cleanup_quarantine {
        reconcile_cleanup_quarantine(instance_mods_dir)?;
    }
    if preflight.state_publication {
        reconcile_state_publication(instance_mods_dir)?;
    }
    Ok(())
}

pub(crate) fn reconcile_managed_inspection_obligations(
    instance_mods_dir: &Path,
    preflight: ManagedInspectionReconciliation,
    state: Option<&CompositionState>,
) -> Result<(), StateError> {
    if preflight.managed_addition {
        reconcile_managed_addition_obligations(instance_mods_dir, state)?;
    }
    if preflight.managed_removal {
        reconcile_managed_removal_obligations(instance_mods_dir, state)?;
    }
    if preflight.rollback_publication {
        reconcile_rollback_metadata(instance_mods_dir)?;
    }
    Ok(())
}

fn managed_addition_reconciliation_required(instance_mods_dir: &Path) -> Result<bool, StateError> {
    let state_root = instance_mods_dir.join(STATE_DIR_NAME);
    let mutation_root = state_root.join(MUTATION_DIR_NAME);
    let root = mutation_root.join(ADDITION_DIR_NAME);
    for path in [&state_root, &mutation_root, &root] {
        validate_managed_recovery_directory(path)?;
    }
    let mut entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    };
    Ok(entries.next().transpose()?.is_some())
}

pub(crate) fn save_state(
    instance_mods_dir: &Path,
    state: &CompositionState,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    validate_state(state)?;
    ensure_instance_mods_directory(instance_mods_dir)?;
    reconcile_state_publication_for_mutation(instance_mods_dir)?;
    let snapshot = PersistedCompositionState {
        schema_version: STATE_SCHEMA_VERSION,
        state: state.clone(),
    };
    let data = serde_json::to_vec_pretty(&snapshot)?;
    if data.len() as u64 > STATE_MAX_BYTES {
        return Err(StateError::InvalidState(
            "performance state exceeds the byte budget".to_string(),
        ));
    }
    let path = lock_file_path(instance_mods_dir);
    let staged = state_staged_path(instance_mods_dir);
    write_exclusive_file(&staged, &data, StatePublicationPhase::Stage)?;
    publish_staged_state(instance_mods_dir, &staged, &path)
}

pub(crate) fn remove_state(instance_mods_dir: &Path) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    reconcile_state_publication_for_mutation(instance_mods_dir)?;
    let path = lock_file_path(instance_mods_dir);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(StateError::InvalidState(
                "performance state is not a regular file".to_string(),
            ));
        }
    }
    let admitted_sha512 = admitted_state_file_sha512(&path)?;
    let marker = state_delete_path(instance_mods_dir);
    write_exclusive_file(&marker, LOCK_DELETE_MARKER, StatePublicationPhase::Stage)?;
    let backup = state_backup_path(instance_mods_dir);
    reserve_backup_exclusive(
        &path,
        &backup,
        StatePublicationPhase::Backup,
        Some(&admitted_sha512),
    )?;
    remove_file_matching_sha512(&backup, &admitted_sha512, STATE_MAX_BYTES)?;
    let marker_sha512 = hex::encode(Sha512::digest(LOCK_DELETE_MARKER));
    remove_file_matching_sha512(&marker, &marker_sha512, LOCK_DELETE_MARKER.len() as u64)
}

pub(crate) fn lock_file_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir.join(LOCK_FILE_NAME)
}

fn state_staged_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir.join(LOCK_STAGED_FILE_NAME)
}

fn state_backup_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir.join(LOCK_BACKUP_FILE_NAME)
}

fn state_delete_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir.join(LOCK_DELETE_FILE_NAME)
}

fn state_publication_reconciliation_required(instance_mods_dir: &Path) -> Result<bool, StateError> {
    let destination = lock_file_path(instance_mods_dir);
    let staged = state_staged_path(instance_mods_dir);
    let backup = state_backup_path(instance_mods_dir);
    let deletion = state_delete_path(instance_mods_dir);
    let deletion_present = read_delete_marker_if_present(&deletion)?;
    let staged_present = path_exists(&staged)?;
    let backup_present = path_exists(&backup)?;
    if !deletion_present && !staged_present && !backup_present {
        return Ok(false);
    }

    read_state_snapshot_if_present(&destination)?;
    read_state_snapshot_if_present(&staged)?;
    read_state_snapshot_if_present(&backup)?;
    Ok(true)
}

fn publication(phase: StatePublicationPhase, source: std::io::Error) -> StateError {
    StateError::Publication { phase, source }
}

fn ensure_instance_mods_directory(instance_mods_dir: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(instance_mods_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(StateError::InvalidState(
            "performance state parent is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(instance_mods_dir)?;
            match fs::symlink_metadata(instance_mods_dir) {
                Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
                Ok(_) => Err(StateError::InvalidState(
                    "performance state parent is not a regular directory".to_string(),
                )),
                Err(error) => Err(StateError::Read(error)),
            }
        }
        Err(error) => Err(StateError::Read(error)),
    }
}

fn write_exclusive_file(
    path: &Path,
    contents: &[u8],
    phase: StatePublicationPhase,
) -> Result<(), StateError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| publication(phase, source))?;
    if let Err(source) = file.write_all(contents).and_then(|()| file.sync_all()) {
        drop(file);
        let cleanup = remove_publication_file(path, StatePublicationPhase::Cleanup);
        return cleanup.and(Err(publication(phase, source)));
    }
    Ok(())
}

fn publish_staged_state(
    instance_mods_dir: &Path,
    staged: &Path,
    destination: &Path,
) -> Result<(), StateError> {
    let backup = state_backup_path(instance_mods_dir);
    let backup_sha512 = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let admitted_sha512 = admitted_state_file_sha512(destination)?;
            reserve_backup_exclusive(
                destination,
                &backup,
                StatePublicationPhase::Backup,
                Some(&admitted_sha512),
            )?;
            Some(admitted_sha512)
        }
        Ok(_) => {
            return Err(StateError::InvalidState(
                "performance state is not a regular file".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(StateError::Read(error)),
    };

    if let Err(source) = fs::rename(staged, destination) {
        if path_exists(&backup)? && !path_exists(destination)? {
            fs::rename(&backup, destination)
                .map_err(|restore| publication(StatePublicationPhase::Reconcile, restore))?;
        }
        return Err(publication(StatePublicationPhase::Publish, source));
    }
    if let Some(backup_sha512) = backup_sha512 {
        remove_file_matching_sha512(&backup, &backup_sha512, STATE_MAX_BYTES)?;
    }
    Ok(())
}

pub(crate) fn reconcile_state_publication(instance_mods_dir: &Path) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    let destination = lock_file_path(instance_mods_dir);
    let staged = state_staged_path(instance_mods_dir);
    let backup = state_backup_path(instance_mods_dir);
    let deletion = state_delete_path(instance_mods_dir);

    let deletion_present = read_delete_marker_if_present(&deletion)?;
    if !deletion_present && !path_exists(&staged)? && !path_exists(&backup)? {
        return Ok(());
    }
    if deletion_present {
        if path_exists(&destination)? {
            let admitted_destination = read_state_snapshot_file(&destination)?;
            if path_exists(&backup)? {
                let admitted_backup = read_state_snapshot_file(&backup)?;
                let destination_identity = admit_file_identity(&destination).map_err(|error| {
                    identity_admission_error(
                        error,
                        StateError::InvalidState(
                            "performance state destination identity cannot be proven".to_string(),
                        ),
                    )
                })?;
                if crate::file_identity::revalidate(
                    &backup,
                    destination_identity.0,
                    destination_identity.1,
                )
                .is_err()
                    || admitted_destination.sha512 != admitted_backup.sha512
                {
                    return Err(StateError::InvalidState(
                        "performance state deletion backup identity is ambiguous".to_string(),
                    ));
                }
                remove_file_matching_sha512(
                    &destination,
                    &admitted_destination.sha512,
                    STATE_MAX_BYTES,
                )?;
            } else {
                reserve_backup_exclusive(
                    &destination,
                    &backup,
                    StatePublicationPhase::Reconcile,
                    Some(&admitted_destination.sha512),
                )?;
            }
        }
        if path_exists(&backup)? {
            let admitted_backup = read_state_snapshot_file(&backup)?;
            remove_file_matching_sha512(&backup, &admitted_backup.sha512, STATE_MAX_BYTES)?;
        }
        let marker_sha512 = hex::encode(Sha512::digest(LOCK_DELETE_MARKER));
        remove_file_matching_sha512(&deletion, &marker_sha512, LOCK_DELETE_MARKER.len() as u64)?;
    }

    let destination_snapshot = read_state_snapshot_if_present(&destination)?;
    let staged_snapshot = read_state_snapshot_if_present(&staged)?;
    let backup_snapshot = read_state_snapshot_if_present(&backup)?;
    match (destination_snapshot, staged_snapshot, backup_snapshot) {
        (Some(destination_admitted), Some(_staged_admitted), Some(backup_admitted)) => {
            let destination_identity = admit_file_identity(&destination).map_err(|error| {
                identity_admission_error(
                    error,
                    StateError::InvalidState(
                        "performance state destination identity cannot be proven".to_string(),
                    ),
                )
            })?;
            if crate::file_identity::revalidate(
                &backup,
                destination_identity.0,
                destination_identity.1,
            )
            .is_err()
                || destination_admitted.sha512 != backup_admitted.sha512
            {
                return Err(StateError::InvalidState(
                    "performance state publication backup identity is ambiguous".to_string(),
                ));
            }
            remove_file_matching_sha512(
                &destination,
                &destination_admitted.sha512,
                STATE_MAX_BYTES,
            )?;
            fs::rename(&staged, &destination)
                .map_err(|source| publication(StatePublicationPhase::Reconcile, source))?;
            remove_file_matching_sha512(&backup, &backup_admitted.sha512, STATE_MAX_BYTES)
        }
        (Some(_), Some(staged_admitted), None) => {
            remove_file_matching_sha512(&staged, &staged_admitted.sha512, STATE_MAX_BYTES)
        }
        (Some(_), None, Some(backup_admitted)) => {
            remove_file_matching_sha512(&backup, &backup_admitted.sha512, STATE_MAX_BYTES)
        }
        (None, Some(_staged_admitted), Some(backup_admitted)) => {
            fs::rename(&staged, &destination)
                .map_err(|source| publication(StatePublicationPhase::Reconcile, source))?;
            remove_file_matching_sha512(&backup, &backup_admitted.sha512, STATE_MAX_BYTES)
        }
        (None, Some(_), None) => fs::rename(&staged, &destination)
            .map_err(|source| publication(StatePublicationPhase::Reconcile, source)),
        (None, None, Some(_)) => fs::rename(&backup, &destination)
            .map_err(|source| publication(StatePublicationPhase::Reconcile, source)),
        (Some(_), None, None) | (None, None, None) => Ok(()),
    }
}

fn reconcile_state_publication_for_mutation(instance_mods_dir: &Path) -> Result<(), StateError> {
    reconcile_state_publication(instance_mods_dir)?;
    load_state_admitted(instance_mods_dir).map(|_| ())
}

fn read_delete_marker_if_present(path: &Path) -> Result<bool, StateError> {
    let Some(data) = read_bounded_regular_file_if_present(path, LOCK_DELETE_MARKER.len() as u64)?
    else {
        return Ok(false);
    };
    if data == LOCK_DELETE_MARKER {
        Ok(true)
    } else {
        Err(StateError::InvalidState(
            "performance state deletion marker ownership cannot be proven".to_string(),
        ))
    }
}

fn read_state_snapshot_if_present(
    path: &Path,
) -> Result<Option<AdmittedPersistedCompositionState>, StateError> {
    let Some(data) = read_bounded_regular_file_if_present(path, STATE_MAX_BYTES)? else {
        return Ok(None);
    };
    let snapshot = serde_json::from_slice::<PersistedCompositionState>(&data)?;
    validate_persisted_state(&snapshot)?;
    Ok(Some(AdmittedPersistedCompositionState {
        snapshot,
        sha512: hex::encode(Sha512::digest(data)),
    }))
}

fn read_state_snapshot_file(path: &Path) -> Result<AdmittedPersistedCompositionState, StateError> {
    read_state_snapshot_if_present(path)?.ok_or_else(|| {
        StateError::InvalidState("performance state disappeared during admission".to_string())
    })
}

fn admitted_state_file_sha512(path: &Path) -> Result<String, StateError> {
    Ok(read_state_snapshot_file(path)?.sha512)
}

fn read_bounded_regular_file_if_present(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, StateError> {
    let admitted = match crate::file_identity::admit(path) {
        Ok(admitted) => admitted,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(StateError::InvalidState(
                "performance state obligation is not a bounded regular file".to_string(),
            ));
        }
        Err(error) => return Err(StateError::Read(error)),
    };
    if admitted.metadata().len() > max_bytes {
        return Err(StateError::InvalidState(
            "performance state obligation is not a bounded regular file".to_string(),
        ));
    }
    let identity = admitted.identity();
    let admitted_len = admitted.metadata().len();
    let mut file = admitted.into_file();
    let mut data = Vec::with_capacity(admitted_len as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut data)?;
    if data.len() as u64 != admitted_len
        || crate::file_identity::revalidate(path, identity, admitted_len).is_err()
    {
        return Err(StateError::InvalidState(
            "performance state bytes changed during admission".to_string(),
        ));
    }
    Ok(Some(data))
}

fn remove_publication_file(path: &Path, phase: StatePublicationPhase) -> Result<(), StateError> {
    remove_owned_file(path).map_err(|error| cleanup_publication_error(phase, error))
}

fn reserve_backup_exclusive(
    source: &Path,
    backup: &Path,
    phase: StatePublicationPhase,
    expected_sha512: Option<&str>,
) -> Result<(), StateError> {
    fs::hard_link(source, backup).map_err(|source| publication(phase, source))?;
    let (source_identity, source_len) = admit_file_identity(source)
        .map_err(|source| publication(StatePublicationPhase::Reconcile, source))?;
    let digest_matches = match expected_sha512 {
        Some(expected) => path_matches_sha512(backup, expected)?,
        None => true,
    };
    if crate::file_identity::revalidate(backup, source_identity, source_len).is_err()
        || !digest_matches
    {
        remove_publication_file(backup, StatePublicationPhase::Cleanup)?;
        return Err(StateError::InvalidState(
            "performance state backup ownership cannot be proven".to_string(),
        ));
    }
    if crate::file_identity::revalidate(source, source_identity, source_len).is_err() {
        remove_publication_file(backup, StatePublicationPhase::Cleanup)?;
        return Err(StateError::InvalidState(
            "performance state destination changed during backup".to_string(),
        ));
    }
    let source_sha512 = match expected_sha512 {
        Some(expected) => expected.to_string(),
        None => hex::encode(bounded_file_sha512(source, source_len)?),
    };
    remove_identity_bound_file(source, source_identity, source_len, &source_sha512)
        .map_err(|error| cleanup_publication_error(phase, error))?;
    Ok(())
}

fn cleanup_publication_error(phase: StatePublicationPhase, error: StateError) -> StateError {
    match error {
        StateError::Read(source) => publication(phase, source),
        error => error,
    }
}

pub(crate) fn save_rollback_snapshot(
    instance_mods_dir: &Path,
    state: &CompositionState,
) -> Result<RollbackSnapshot, StateError> {
    validate_state(state)?;
    save_rollback_snapshot_target(
        instance_mods_dir,
        RollbackSnapshotState::ManagedComposition {
            state: state.clone(),
        },
    )
}

pub(crate) fn save_absent_rollback_snapshot(
    instance_mods_dir: &Path,
) -> Result<RollbackSnapshot, StateError> {
    save_rollback_snapshot_target(instance_mods_dir, RollbackSnapshotState::ManagedStateAbsent)
}

fn save_rollback_snapshot_target(
    instance_mods_dir: &Path,
    target: RollbackSnapshotState,
) -> Result<RollbackSnapshot, StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    let snapshot_id = new_rollback_snapshot_id();
    let planned = match &target {
        RollbackSnapshotState::ManagedStateAbsent => PlannedRollbackSnapshot {
            artifacts: Vec::new(),
            total_bytes: 0,
        },
        RollbackSnapshotState::ManagedComposition { state } => {
            plan_rollback_artifacts(instance_mods_dir, state, &snapshot_id)?
        }
    };
    finish_rollback_retention(instance_mods_dir)?;

    let snapshot = RollbackSnapshot {
        id: snapshot_id.clone(),
        schema_version: ROLLBACK_SCHEMA_VERSION,
        created_at: Utc::now().to_rfc3339(),
        target,
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
    finish_rollback_retention(instance_mods_dir)?;
    Ok(snapshot)
}

pub(crate) async fn save_rollback_snapshot_async(
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

pub(crate) async fn save_absent_rollback_snapshot_async(
    instance_mods_dir: &Path,
) -> Result<RollbackSnapshot, StateError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    tokio::task::spawn_blocking(move || save_absent_rollback_snapshot(&instance_mods_dir))
        .await
        .map_err(|_| {
            StateError::Read(std::io::Error::other(
                "absent rollback snapshot task stopped before reporting its result",
            ))
        })?
}

struct PlannedRollbackSnapshot {
    artifacts: Vec<PlannedRollbackArtifact>,
    total_bytes: u64,
}

struct PlannedRollbackArtifact {
    source_path: PathBuf,
    staged_path: PathBuf,
    stored_path: PathBuf,
    expected_bytes: u64,
    metadata: RollbackArtifact,
}

fn plan_rollback_artifacts(
    instance_mods_dir: &Path,
    state: &CompositionState,
    snapshot_id: &str,
) -> Result<PlannedRollbackSnapshot, StateError> {
    let mut admitted = Vec::with_capacity(state.installed_mods.len());
    let mut total_bytes = 0_u64;
    for (index, installed) in state.installed_mods.iter().enumerate() {
        let source_path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
        let source_metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StateError::InvalidRollback(format!(
                    "tracked rollback source {} is missing",
                    installed.filename
                )));
            }
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
        admitted.push((index, installed, source_path, expected_bytes));
    }

    let mut artifacts = Vec::with_capacity(admitted.len());
    for (index, installed, source_path, expected_bytes) in admitted {
        if !managed_artifact_matches(instance_mods_dir, installed)? {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "rollback source bytes do not match the recorded ownership digest"
                    .to_string(),
            });
        }
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
    let staged_filename = format!("{stored_filename}.new.tmp");
    validate_managed_filename(&stored_filename)?;
    validate_managed_filename(&staged_filename)?;
    Ok(PlannedRollbackArtifact {
        source_path,
        staged_path: rollback_files_dir_path(instance_mods_dir).join(staged_filename),
        stored_path: rollback_files_dir_path(instance_mods_dir).join(&stored_filename),
        expected_bytes,
        metadata: RollbackArtifact {
            filename: installed.filename.clone(),
            stored_filename,
            project_id: installed.project_id.clone(),
            version_id: installed.version_id.clone(),
            ownership_class: installed.ownership_class,
            sha512: installed.integrity.sha512.clone(),
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
    let mut count = 0_usize;
    let rollback_dir = rollback_dir_path(instance_mods_dir);
    if !rollback_dir.exists() {
        return Ok(0);
    }
    total_directory_files(&rollback_dir, false, &mut total, &mut count)?;
    for path in [
        rollback_files_dir_path(instance_mods_dir),
        rollback_history_dir_path(instance_mods_dir),
        rollback_tmp_dir_path(instance_mods_dir),
    ] {
        total_directory_files(&path, true, &mut total, &mut count)?;
    }
    Ok(total)
}

fn total_directory_files(
    directory: &Path,
    allow_no_subdirectories: bool,
    total: &mut u64,
    count: &mut usize,
) -> Result<(), StateError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(count, "rollback storage entries")?;
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

enum RollbackMetadataPublicationFailure {
    PrePublish(StateError),
    Indeterminate(StateError),
}

fn commit_rollback_snapshot(
    instance_mods_dir: &Path,
    planned: &PlannedRollbackSnapshot,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    ensure_rollback_internal_roots(instance_mods_dir)?;
    let history_path = rollback_history_file_path(instance_mods_dir, &snapshot.id);
    preflight_rollback_candidate_namespace(instance_mods_dir, planned, &history_path)?;
    let history_temp = stage_new_rollback_snapshot(&history_path, snapshot)?;
    if let Err(error) = sync_rollback_directory(
        &rollback_history_dir_path(instance_mods_dir),
        RollbackDurabilityPoint::HistoryStaged,
    ) {
        cleanup_unpublished_rollback_candidate(instance_mods_dir, &history_temp, snapshot)?;
        return Err(error);
    }
    for artifact in &planned.artifacts {
        if let Err(failure) = copy_rollback_artifact(artifact) {
            if !failure.published {
                cleanup_unpublished_rollback_candidate(instance_mods_dir, &history_temp, snapshot)?;
            }
            return Err(failure.error);
        }
    }
    if let Err(error) = sync_rollback_directory(
        &rollback_files_dir_path(instance_mods_dir),
        RollbackDurabilityPoint::ArtifactCopiesPublished,
    ) {
        cleanup_unpublished_rollback_candidate(instance_mods_dir, &history_temp, snapshot)?;
        return Err(error);
    }
    if let Err(failure) = durable_rollback_rename(
        &history_temp,
        &history_path,
        RollbackDurabilityPoint::HistoryPublished,
    ) {
        if !failure.renamed {
            cleanup_unpublished_rollback_candidate(instance_mods_dir, &history_temp, snapshot)?;
        }
        return Err(failure.error);
    }
    write_rollback_snapshot(&rollback_file_path(instance_mods_dir), snapshot).map_err(|failure| {
        match failure {
            RollbackMetadataPublicationFailure::PrePublish(error)
            | RollbackMetadataPublicationFailure::Indeterminate(error) => error,
        }
    })
}

fn preflight_rollback_candidate_namespace(
    instance_mods_dir: &Path,
    planned: &PlannedRollbackSnapshot,
    history_path: &Path,
) -> Result<(), StateError> {
    let history_temp = history_path.with_extension("json.new.tmp");
    preflight_rollback_candidate_directory(
        &rollback_history_dir_path(instance_mods_dir),
        [history_path, history_temp.as_path()],
    )?;
    preflight_rollback_candidate_directory(
        &rollback_files_dir_path(instance_mods_dir),
        planned.artifacts.iter().flat_map(|artifact| {
            [
                artifact.stored_path.as_path(),
                artifact.staged_path.as_path(),
            ]
        }),
    )
}

fn preflight_rollback_candidate_directory<'a>(
    directory: &Path,
    candidates: impl IntoIterator<Item = &'a Path>,
) -> Result<(), StateError> {
    let candidate_names = candidates
        .into_iter()
        .map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_ascii_lowercase())
                .ok_or_else(|| {
                    StateError::InvalidRollback(
                        "rollback candidate namespace name is invalid".to_string(),
                    )
                })
        })
        .collect::<Result<HashSet<_>, _>>()?;
    let entries = fs::read_dir(directory)?;
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "rollback candidate namespace entries")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            StateError::InvalidRollback(
                "rollback candidate namespace contains an invalid name".to_string(),
            )
        })?;
        if candidate_names.contains(&name.to_ascii_lowercase()) {
            return Err(StateError::InvalidRollback(
                "rollback candidate namespace is already occupied".to_string(),
            ));
        }
    }
    Ok(())
}

struct RollbackArtifactCopyFailure {
    error: StateError,
    published: bool,
}

fn copy_rollback_artifact(
    artifact: &PlannedRollbackArtifact,
) -> Result<(), RollbackArtifactCopyFailure> {
    stage_rollback_artifact_copy(artifact).map_err(|error| RollbackArtifactCopyFailure {
        error,
        published: false,
    })?;
    match durable_rollback_rename(
        &artifact.staged_path,
        &artifact.stored_path,
        RollbackDurabilityPoint::ArtifactPublished,
    ) {
        Ok(()) => Ok(()),
        Err(failure) if failure.renamed => Err(RollbackArtifactCopyFailure {
            error: failure.error,
            published: true,
        }),
        Err(failure) => {
            remove_owned_file(&artifact.staged_path).map_err(|error| {
                RollbackArtifactCopyFailure {
                    error,
                    published: false,
                }
            })?;
            Err(RollbackArtifactCopyFailure {
                error: failure.error,
                published: false,
            })
        }
    }
}

fn stage_rollback_artifact_copy(artifact: &PlannedRollbackArtifact) -> Result<(), StateError> {
    let admitted_source = crate::file_identity::admit(&artifact.source_path).map_err(|error| {
        identity_admission_error(
            error,
            StateError::InvalidRollback(format!(
                "managed rollback source {} changed before copy",
                artifact.metadata.filename
            )),
        )
    })?;
    if admitted_source.metadata().len() != artifact.expected_bytes {
        return Err(StateError::InvalidRollback(format!(
            "managed rollback source {} changed before copy",
            artifact.metadata.filename
        )));
    }
    let source_identity = admitted_source.identity();
    let source = admitted_source.into_file();
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&artifact.staged_path)?;
    let result = (|| {
        let copied = std::io::copy(
            &mut source.take(artifact.expected_bytes.saturating_add(1)),
            &mut destination,
        )?;
        destination.sync_all()?;
        Ok::<u64, std::io::Error>(copied)
    })();
    let copied = match result {
        Ok(copied) => copied,
        Err(error) => {
            drop(destination);
            remove_owned_file(&artifact.staged_path)?;
            return Err(StateError::Read(error));
        }
    };
    if copied != artifact.expected_bytes {
        drop(destination);
        remove_owned_file(&artifact.staged_path)?;
        return Err(StateError::InvalidRollback(format!(
            "managed rollback source {} changed during copy",
            artifact.metadata.filename
        )));
    }
    drop(destination);
    let (source_digest, revalidated_source_identity) =
        admit_bounded_file_sha512(&artifact.source_path, artifact.expected_bytes)?;
    let (stored_digest, stored_identity) =
        admit_bounded_file_sha512(&artifact.staged_path, artifact.expected_bytes)?;
    let expected_digest = &artifact.metadata.sha512;
    if revalidated_source_identity != Some(source_identity)
        || stored_identity.is_none()
        || stored_identity == Some(source_identity)
        || source_digest.is_empty()
        || stored_digest != source_digest
        || !hex::encode(stored_digest).eq_ignore_ascii_case(expected_digest)
    {
        remove_owned_file(&artifact.staged_path)?;
        return Err(StateError::InvalidIntegrity {
            filename: artifact.metadata.filename.clone(),
            reason: "rollback copy does not match the recorded ownership digest".to_string(),
        });
    }
    Ok(())
}

fn cleanup_created_rollback_artifacts(paths: &[&Path]) -> Result<(), StateError> {
    for path in paths {
        remove_owned_file(path)?;
    }
    Ok(())
}

fn cleanup_unpublished_rollback_candidate(
    instance_mods_dir: &Path,
    history_temp: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    if read_rollback_snapshot_file(history_temp)? != *snapshot {
        return Err(StateError::InvalidRollback(
            "rollback candidate manifest changed before cleanup".to_string(),
        ));
    }
    cleanup_abandoned_snapshot_artifacts(instance_mods_dir, snapshot)?;
    sync_rollback_directory(
        &rollback_files_dir_path(instance_mods_dir),
        RollbackDurabilityPoint::ArtifactCopiesCleaned,
    )?;
    remove_rollback_history_candidate(instance_mods_dir, history_temp)
}

fn remove_rollback_history_candidate(
    instance_mods_dir: &Path,
    history_temp: &Path,
) -> Result<(), StateError> {
    remove_owned_file(history_temp)?;
    sync_rollback_directory(
        &rollback_history_dir_path(instance_mods_dir),
        RollbackDurabilityPoint::HistoryCandidateCleaned,
    )
}

fn finish_rollback_retention(instance_mods_dir: &Path) -> Result<(), StateError> {
    reconcile_restore_stage_temps(instance_mods_dir)?;
    reconcile_prune_artifact_temps(instance_mods_dir)?;
    cleanup_proven_history_temps(instance_mods_dir)?;
    prune_rollback_history(instance_mods_dir)?;
    cleanup_proven_latest_temp(instance_mods_dir)
}

fn reconcile_prune_artifact_temps(instance_mods_dir: &Path) -> Result<(), StateError> {
    let files_dir = rollback_files_dir_path(instance_mods_dir);
    let entries = match fs::read_dir(&files_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "rollback prune recovery entries")?;
        let Some(filename) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some((original_name, digest)) = filename.rsplit_once(".prune-") else {
            continue;
        };
        let Some(digest) = digest.strip_suffix(".tmp").filter(|digest| {
            digest.len() == 128 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) else {
            continue;
        };
        if !path_matches_sha512(&entry.path(), digest)? {
            return Err(StateError::InvalidRollback(
                "rollback prune obligation ownership cannot be proven".to_string(),
            ));
        }
        let original = files_dir.join(original_name);
        let snapshot_id = original_name
            .strip_suffix(".bin")
            .and_then(|stem| stem.rsplit_once('-').map(|(snapshot_id, _)| snapshot_id))
            .filter(|snapshot_id| validate_rollback_snapshot_id(snapshot_id).is_ok())
            .ok_or_else(|| {
                StateError::InvalidRollback(
                    "rollback prune obligation identity is invalid".to_string(),
                )
            })?;
        let history_exists =
            path_exists(&rollback_history_file_path(instance_mods_dir, snapshot_id))?;
        if history_exists && path_exists(&original)? {
            if !path_matches_sha512(&original, digest)? {
                return Err(StateError::InvalidRollback(
                    "rollback prune source ownership cannot be proven".to_string(),
                ));
            }
            remove_file_matching_sha512(&entry.path(), digest, MANAGED_ARTIFACT_MAX_BYTES)?;
        } else if history_exists {
            fs::rename(entry.path(), original)?;
        } else {
            remove_file_matching_sha512(&entry.path(), digest, MANAGED_ARTIFACT_MAX_BYTES)?;
        }
    }
    Ok(())
}

pub(crate) fn load_rollback_snapshot(
    instance_mods_dir: &Path,
) -> Result<Option<RollbackSnapshot>, StateError> {
    reconcile_managed_storage(instance_mods_dir)?;
    load_rollback_snapshot_admitted(instance_mods_dir)
}

pub(crate) fn load_rollback_snapshot_admitted(
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

pub(crate) async fn load_rollback_snapshot_async(
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

pub(crate) fn load_rollback_snapshot_by_id(
    instance_mods_dir: &Path,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    reconcile_managed_storage(instance_mods_dir)?;
    load_rollback_snapshot_by_id_admitted(instance_mods_dir, snapshot_id)
}

pub(crate) fn load_rollback_snapshot_by_id_admitted(
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

pub(crate) async fn load_rollback_snapshot_by_id_async(
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

pub(crate) fn list_rollback_snapshots_admitted(
    instance_mods_dir: &Path,
) -> Result<Vec<RollbackSnapshotSummary>, StateError> {
    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    Ok(snapshots
        .into_iter()
        .map(|record| {
            let target = record.snapshot.target_kind();
            let state = record.snapshot.state();
            RollbackSnapshotSummary {
                id: record.snapshot.id.clone(),
                created_at: record.snapshot.created_at.clone(),
                target,
                composition_id: state.map(|state| state.composition_id.clone()),
                tier: state.map(|state| state.tier),
                installed_count: state.map_or(0, |state| state.installed_mods.len()),
                artifact_count: record.snapshot.artifacts.len(),
                ownership_class: OwnershipClass::CompositionManaged,
                rollback_available: true,
                latest: record.latest,
            }
        })
        .collect())
}

pub(crate) fn restore_rollback_snapshot(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<ManagedRollbackOutcome, StateError> {
    restore_rollback_snapshot_classified(instance_mods_dir, snapshot)
        .map_err(RollbackRestoreError::into_state_error)
}

#[derive(Debug)]
pub(crate) enum RollbackRestoreError {
    Definite(StateError),
    Indeterminate(StateError),
}

impl RollbackRestoreError {
    pub(crate) fn into_state_error(self) -> StateError {
        match self {
            Self::Definite(error) | Self::Indeterminate(error) => error,
        }
    }
}

pub(crate) fn restore_rollback_snapshot_classified(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<ManagedRollbackOutcome, RollbackRestoreError> {
    require_cleanup_quarantine_empty(instance_mods_dir).map_err(RollbackRestoreError::Definite)?;
    validate_rollback_snapshot(snapshot).map_err(RollbackRestoreError::Definite)?;

    let snapshot_filenames: HashSet<String> = snapshot
        .state()
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
        .map(|installed| installed.filename.clone())
        .collect();
    reconcile_state_publication(instance_mods_dir).map_err(RollbackRestoreError::Indeterminate)?;
    let current_state =
        load_state_admitted(instance_mods_dir).map_err(RollbackRestoreError::Definite)?;
    reconcile_managed_removal_obligations(instance_mods_dir, current_state.as_ref())
        .map_err(RollbackRestoreError::Indeterminate)?;
    reconcile_rollback_metadata(instance_mods_dir).map_err(RollbackRestoreError::Indeterminate)?;
    let current_artifacts = managed_artifacts(current_state.as_ref());
    let superseded = current_state
        .as_ref()
        .map(|state| {
            state
                .installed_mods
                .iter()
                .filter(|installed| !snapshot_filenames.contains(&installed.filename))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    reconcile_restore_stage_temps(instance_mods_dir)
        .map_err(RollbackRestoreError::Indeterminate)?;
    let restore_targets =
        prepare_rollback_restore_targets(instance_mods_dir, snapshot, &current_artifacts)
            .map_err(RollbackRestoreError::Definite)?;
    stage_rollback_restore_targets(&restore_targets)
        .map_err(RollbackRestoreError::Indeterminate)?;
    for installed in &superseded {
        if let Err(error) = stage_managed_artifact_removal(instance_mods_dir, installed) {
            load_state(instance_mods_dir).map_err(RollbackRestoreError::Indeterminate)?;
            cleanup_rollback_restore_targets(&restore_targets)
                .map_err(RollbackRestoreError::Indeterminate)?;
            return Err(RollbackRestoreError::Indeterminate(error));
        }
    }
    let publication = (|| {
        for target in &restore_targets {
            publish_rollback_restore_target(target)?;
        }
        match snapshot.state() {
            Some(state) => save_state(instance_mods_dir, state)?,
            None => remove_state(instance_mods_dir)?,
        }
        Ok::<(), StateError>(())
    })();
    if let Err(error) = publication {
        compensate_rollback_restore_targets(&restore_targets)
            .map_err(RollbackRestoreError::Indeterminate)?;
        load_state(instance_mods_dir).map_err(RollbackRestoreError::Indeterminate)?;
        cleanup_rollback_restore_targets(&restore_targets)
            .map_err(RollbackRestoreError::Indeterminate)?;
        return Err(RollbackRestoreError::Indeterminate(error));
    }
    reconcile_managed_addition_obligations(instance_mods_dir, snapshot.state())
        .map_err(RollbackRestoreError::Indeterminate)?;
    cleanup_rollback_restore_backups(&restore_targets)
        .map_err(RollbackRestoreError::Indeterminate)?;
    cleanup_rollback_restore_targets(&restore_targets)
        .map_err(RollbackRestoreError::Indeterminate)?;
    for installed in &superseded {
        settle_managed_artifact_removal(instance_mods_dir, installed)
            .map_err(RollbackRestoreError::Indeterminate)?;
    }
    Ok(match snapshot.state() {
        Some(state) => ManagedRollbackOutcome::ManagedComposition(state.clone()),
        None => ManagedRollbackOutcome::ManagedStateAbsent,
    })
}

pub(crate) async fn restore_rollback_snapshot_classified_async(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<ManagedRollbackOutcome, RollbackRestoreError> {
    let instance_mods_dir = instance_mods_dir.to_path_buf();
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || {
        restore_rollback_snapshot_classified(&instance_mods_dir, &snapshot)
    })
    .await
    .map_err(|_| {
        RollbackRestoreError::Indeterminate(StateError::Read(std::io::Error::other(
            "rollback restore task stopped before reporting its result",
        )))
    })?
}

pub(crate) fn managed_artifact_path(
    instance_mods_dir: &Path,
    filename: &str,
) -> Result<PathBuf, StateError> {
    validate_managed_filename(filename)?;
    Ok(instance_mods_dir.join(filename))
}

pub(crate) fn managed_artifact_matches(
    instance_mods_dir: &Path,
    installed: &InstalledMod,
) -> Result<bool, StateError> {
    let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    };
    if !metadata.file_type().is_file() || metadata.len() != installed.size {
        return Ok(false);
    }
    path_matches_sha512(&path, &installed.integrity.sha512)
}

pub(crate) fn stage_managed_artifact_removal(
    instance_mods_dir: &Path,
    installed: &InstalledMod,
) -> Result<PathBuf, StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
    let backup = removal_backup_path(instance_mods_dir, installed);
    if path_exists(&backup)? {
        if !path_matches_sha512(&backup, &installed.integrity.sha512)? {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed removal backup ownership cannot be proven".to_string(),
            });
        }
        if !path_exists(&path)? {
            return Ok(backup);
        }
        if !path_matches_sha512(&path, &installed.integrity.sha512)? {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed removal destination ownership cannot be proven".to_string(),
            });
        }
        remove_file_matching_sha512(
            &backup,
            &installed.integrity.sha512,
            MANAGED_ARTIFACT_MAX_BYTES,
        )?;
    }
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(removal_backup_path(instance_mods_dir, installed));
        }
        Err(error) => return Err(StateError::Read(error)),
        Ok(_) => {}
    }
    if !managed_artifact_matches(instance_mods_dir, installed)? {
        return Err(StateError::InvalidIntegrity {
            filename: installed.filename.clone(),
            reason: "current bytes do not match the recorded ownership digest".to_string(),
        });
    }
    let parent = backup.parent().ok_or_else(|| {
        StateError::InvalidState("managed removal backup path is invalid".to_string())
    })?;
    ensure_mutation_directory_tree(instance_mods_dir, parent)?;
    reserve_backup_exclusive(
        &path,
        &backup,
        StatePublicationPhase::Backup,
        Some(&installed.integrity.sha512),
    )?;
    Ok(backup)
}

pub(crate) fn stage_managed_artifact_addition(
    instance_mods_dir: &Path,
    filename: &str,
    sha512: &str,
    source_path: &Path,
) -> Result<PathBuf, StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    validate_managed_filename(filename)?;
    if !is_valid_sha512(sha512) {
        return Err(StateError::InvalidIntegrity {
            filename: filename.to_string(),
            reason: "managed addition digest is invalid".to_string(),
        });
    }
    if !path_matches_sha512(source_path, sha512)? {
        return Err(StateError::InvalidIntegrity {
            filename: filename.to_string(),
            reason: "managed addition source ownership cannot be proven".to_string(),
        });
    }
    let digest = sha512.to_ascii_lowercase();
    let obligation = managed_artifact_addition_path(instance_mods_dir, filename, &digest)?;
    let digest_root = obligation
        .parent()
        .expect("managed addition obligation always has a digest parent")
        .to_path_buf();
    let addition_root = digest_root
        .parent()
        .expect("managed addition digest always has an addition parent")
        .to_path_buf();
    let mutation_root = addition_root
        .parent()
        .expect("managed addition root always has a mutation parent")
        .to_path_buf();
    let state_root = mutation_root
        .parent()
        .expect("managed mutation root always has a state parent")
        .to_path_buf();
    for path in [&state_root, &mutation_root, &addition_root, &digest_root] {
        ensure_recovery_directory(path)?;
    }
    match fs::hard_link(source_path, &obligation) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let (identity, len) = admit_file_identity(source_path).map_err(|error| {
                identity_admission_error(
                    error,
                    StateError::InvalidState(
                        "managed addition obligation conflicts with existing ownership".to_string(),
                    ),
                )
            })?;
            if crate::file_identity::revalidate(&obligation, identity, len).is_err()
                || !path_matches_sha512(&obligation, &digest)?
            {
                return Err(StateError::InvalidState(
                    "managed addition obligation conflicts with existing ownership".to_string(),
                ));
            }
        }
        Err(error) => return Err(StateError::Read(error)),
    }
    let (identity, len) = admit_file_identity(source_path).map_err(|error| {
        identity_admission_error(
            error,
            StateError::InvalidState(
                "managed addition obligation identity cannot be proven".to_string(),
            ),
        )
    })?;
    if crate::file_identity::revalidate(&obligation, identity, len).is_err()
        || !path_matches_sha512(&obligation, &digest)?
    {
        return Err(StateError::InvalidState(
            "managed addition obligation identity cannot be proven".to_string(),
        ));
    }
    Ok(obligation)
}

fn managed_artifact_addition_path(
    instance_mods_dir: &Path,
    filename: &str,
    sha512: &str,
) -> Result<PathBuf, StateError> {
    validate_managed_filename(filename)?;
    if !is_valid_sha512(sha512) {
        return Err(StateError::InvalidIntegrity {
            filename: filename.to_string(),
            reason: "managed addition digest is invalid".to_string(),
        });
    }
    Ok(instance_mods_dir
        .join(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(ADDITION_DIR_NAME)
        .join(sha512.to_ascii_lowercase())
        .join(filename))
}

pub(crate) fn prepare_managed_artifact_addition(
    instance_mods_dir: &AnchoredDirectory,
    installed: &InstalledMod,
) -> Result<ManagedArtifactAdditionObligation, StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir.path())?;
    managed_artifact_addition_path(
        instance_mods_dir.path(),
        &installed.filename,
        &installed.integrity.sha512,
    )?;
    let state_root = instance_mods_dir.open_or_create_child(STATE_DIR_NAME)?;
    let mutation_root = state_root.open_or_create_child(MUTATION_DIR_NAME)?;
    let addition_root = mutation_root.open_or_create_child(ADDITION_DIR_NAME)?;
    let digest = installed.integrity.sha512.to_ascii_lowercase();
    let parent = addition_root.open_or_create_child(&digest)?;
    let path = parent.path().join(&installed.filename);
    if path_exists(&path)? {
        return Err(StateError::InvalidState(
            "managed addition obligation already exists".to_string(),
        ));
    }
    Ok(ManagedArtifactAdditionObligation {
        root_relative_path: PathBuf::from(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join(ADDITION_DIR_NAME)
            .join(digest)
            .join(&installed.filename),
        path,
        parent,
        filename: installed.filename.clone(),
    })
}

pub(crate) struct ManagedArtifactAdditionObligation {
    root_relative_path: PathBuf,
    path: PathBuf,
    parent: AnchoredDirectory,
    filename: String,
}

impl ManagedArtifactAdditionObligation {
    pub(crate) fn parent(&self) -> &AnchoredDirectory {
        &self.parent
    }

    pub(crate) fn filename(&self) -> &str {
        &self.filename
    }
}

pub(crate) fn publish_managed_artifact_addition(
    instance_mods_dir: &Path,
    installed: &InstalledMod,
    obligation: &ManagedArtifactAdditionObligation,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    managed_artifact_addition_path(
        instance_mods_dir,
        &installed.filename,
        &installed.integrity.sha512,
    )?;
    let expected_relative = PathBuf::from(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(ADDITION_DIR_NAME)
        .join(installed.integrity.sha512.to_ascii_lowercase())
        .join(&installed.filename);
    if obligation.filename != installed.filename
        || obligation.root_relative_path != expected_relative
        || obligation.path != obligation.parent.path().join(&obligation.filename)
    {
        return Err(StateError::InvalidState(
            "managed addition obligation path does not match its artifact".to_string(),
        ));
    }
    let (identity, len) = admit_file_identity(&obligation.path).map_err(|error| {
        identity_admission_error(
            error,
            StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed addition ownership cannot be admitted".to_string(),
            },
        )
    })?;
    if len != installed.size || !path_matches_sha512(&obligation.path, &installed.integrity.sha512)?
    {
        return Err(StateError::InvalidIntegrity {
            filename: installed.filename.clone(),
            reason: "managed addition bytes do not match sealed metadata".to_string(),
        });
    }
    let final_path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
    fs::hard_link(&obligation.path, &final_path)?;
    let final_metadata = fs::symlink_metadata(&final_path)?;
    if !final_metadata.file_type().is_file()
        || final_metadata.len() != installed.size
        || crate::file_identity::revalidate(&final_path, identity, len).is_err()
        || !path_matches_sha512(&final_path, &installed.integrity.sha512)?
    {
        return Err(StateError::InvalidIntegrity {
            filename: installed.filename.clone(),
            reason: "managed addition publication could not be proven".to_string(),
        });
    }
    Ok(())
}

struct AdmittedManagedAddition {
    path: PathBuf,
    identity: crate::file_identity::FileIdentity,
    len: u64,
    digest: String,
    filename: String,
    tracked: bool,
}

pub(crate) fn reconcile_managed_addition_obligations(
    instance_mods_dir: &Path,
    current_state: Option<&CompositionState>,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    let state_root = instance_mods_dir.join(STATE_DIR_NAME);
    let mutation_root = state_root.join(MUTATION_DIR_NAME);
    let root = mutation_root.join(ADDITION_DIR_NAME);
    for path in [&state_root, &mutation_root, &root] {
        validate_managed_recovery_directory(path)?;
    }
    let digest_dirs = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    let mut digest_dirs_to_remove = Vec::new();
    let mut admitted = Vec::new();
    let mut filenames = HashSet::new();
    for digest_dir in digest_dirs {
        let digest_dir = digest_dir?;
        admit_recovery_entry(&mut count, "managed addition obligations")?;
        if !digest_dir.file_type()?.is_dir() {
            return Err(StateError::InvalidState(
                "managed addition digest path is not a directory".to_string(),
            ));
        }
        validate_reserved_directory_metadata(&fs::symlink_metadata(digest_dir.path())?)?;
        let digest = digest_dir
            .file_name()
            .to_str()
            .filter(|value| is_valid_sha512(value))
            .ok_or_else(|| {
                StateError::InvalidState("managed addition digest is invalid".to_string())
            })?
            .to_ascii_lowercase();
        for entry in fs::read_dir(digest_dir.path())? {
            let entry = entry?;
            admit_recovery_entry(&mut count, "managed addition obligations")?;
            let filename = entry
                .file_name()
                .to_str()
                .ok_or_else(|| StateError::InvalidFilename("managed addition".to_string()))?
                .to_string();
            validate_managed_filename(&filename)?;
            if !filenames.insert(filename.to_ascii_lowercase()) {
                return Err(StateError::InvalidState(
                    "managed addition obligations contain duplicate or case-colliding filenames"
                        .to_string(),
                ));
            }
            let obligation_identity = admit_file_identity(&entry.path()).map_err(|error| {
                identity_admission_error(
                    error,
                    StateError::InvalidIntegrity {
                        filename: filename.clone(),
                        reason: "managed addition obligation ownership cannot be proven"
                            .to_string(),
                    },
                )
            })?;
            if !path_matches_sha512(&entry.path(), &digest)?
                || crate::file_identity::revalidate(
                    &entry.path(),
                    obligation_identity.0,
                    obligation_identity.1,
                )
                .is_err()
            {
                return Err(StateError::InvalidIntegrity {
                    filename,
                    reason: "managed addition obligation ownership cannot be proven".to_string(),
                });
            }
            let tracked = current_state.is_some_and(|state| {
                state.installed_mods.iter().any(|installed| {
                    installed.filename == filename
                        && installed.integrity.sha512.eq_ignore_ascii_case(&digest)
                })
            });
            let final_path = managed_artifact_path(instance_mods_dir, &filename)?;
            if tracked {
                match fs::symlink_metadata(&final_path) {
                    Ok(final_metadata) => {
                        if !final_metadata.file_type().is_file()
                            || crate::file_identity::revalidate(
                                &final_path,
                                obligation_identity.0,
                                obligation_identity.1,
                            )
                            .is_err()
                            || !path_matches_sha512(&final_path, &digest)?
                        {
                            return Err(StateError::InvalidIntegrity {
                                filename,
                                reason: "managed addition destination ownership cannot be proven"
                                    .to_string(),
                            });
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(StateError::Read(error)),
                }
            }
            admitted.push(AdmittedManagedAddition {
                path: entry.path(),
                identity: obligation_identity.0,
                len: obligation_identity.1,
                digest: digest.clone(),
                filename,
                tracked,
            });
        }
        digest_dirs_to_remove.push(digest_dir.path());
    }

    if admitted.is_empty() {
        for digest_dir in digest_dirs_to_remove {
            fs::remove_dir(digest_dir)?;
        }
        fs::remove_dir(root)?;
        return Ok(());
    }

    for obligation in &admitted {
        let final_path = managed_artifact_path(instance_mods_dir, &obligation.filename)?;
        match fs::symlink_metadata(&final_path) {
            Ok(final_metadata) => {
                let exact_alias = final_metadata.file_type().is_file()
                    && crate::file_identity::revalidate(
                        &final_path,
                        obligation.identity,
                        obligation.len,
                    )
                    .is_ok()
                    && path_matches_sha512(&final_path, &obligation.digest)?;
                if obligation.tracked && !exact_alias {
                    return Err(StateError::InvalidIntegrity {
                        filename: obligation.filename.clone(),
                        reason: "managed addition destination changed after admission".to_string(),
                    });
                }
                if !obligation.tracked && exact_alias {
                    remove_identity_bound_file(
                        &final_path,
                        obligation.identity,
                        obligation.len,
                        &obligation.digest,
                    )?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && obligation.tracked => {
                fs::hard_link(&obligation.path, &final_path)?;
                let final_metadata = fs::symlink_metadata(&final_path)?;
                if !final_metadata.file_type().is_file()
                    || crate::file_identity::revalidate(
                        &final_path,
                        obligation.identity,
                        obligation.len,
                    )
                    .is_err()
                    || !path_matches_sha512(&final_path, &obligation.digest)?
                {
                    return Err(StateError::InvalidIntegrity {
                        filename: obligation.filename.clone(),
                        reason: "managed addition destination reconstruction failed".to_string(),
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StateError::Read(error)),
        }
        remove_identity_bound_file(
            &obligation.path,
            obligation.identity,
            obligation.len,
            &obligation.digest,
        )?;
    }
    for digest_dir in digest_dirs_to_remove {
        fs::remove_dir(digest_dir)?;
    }
    fs::remove_dir(root)?;
    Ok(())
}

pub(crate) fn settle_managed_artifact_removal(
    instance_mods_dir: &Path,
    installed: &InstalledMod,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    let backup = removal_backup_path(instance_mods_dir, installed);
    if path_exists(&backup)? {
        if !path_matches_sha512(&backup, &installed.integrity.sha512)? {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed removal backup ownership cannot be proven".to_string(),
            });
        }
        remove_file_matching_sha512(
            &backup,
            &installed.integrity.sha512,
            MANAGED_ARTIFACT_MAX_BYTES,
        )?;
    }
    let digest_dir = backup.parent().ok_or_else(|| {
        StateError::InvalidState("managed removal backup has no digest directory".to_string())
    })?;
    settle_empty_removal_digest_directory(digest_dir)?;
    Ok(())
}

fn settle_empty_removal_digest_directory(digest_dir: &Path) -> Result<(), StateError> {
    let root = digest_dir.parent().ok_or_else(|| {
        StateError::InvalidState("managed removal digest has no parent".to_string())
    })?;
    let digest = digest_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_valid_sha512(name))
        .ok_or_else(|| StateError::InvalidState("managed removal digest is invalid".to_string()))?;
    match axial_minecraft::managed_path::AnchoredDirectory::open(root) {
        Ok(root) => {
            root.remove_empty_child(digest)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn removal_backup_path(instance_mods_dir: &Path, installed: &InstalledMod) -> PathBuf {
    instance_mods_dir
        .join(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(REMOVAL_DIR_NAME)
        .join(&installed.integrity.sha512)
        .join(&installed.filename)
}

fn ensure_mutation_directory_tree(instance_mods_dir: &Path, leaf: &Path) -> Result<(), StateError> {
    for path in [
        instance_mods_dir.join(STATE_DIR_NAME),
        instance_mods_dir
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME),
        instance_mods_dir
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join(REMOVAL_DIR_NAME),
        leaf.to_path_buf(),
    ] {
        ensure_managed_directory(&path)?;
    }
    Ok(())
}

fn managed_removal_reconciliation_required(instance_mods_dir: &Path) -> Result<bool, StateError> {
    let state_root = instance_mods_dir.join(STATE_DIR_NAME);
    let mutation_root = state_root.join(MUTATION_DIR_NAME);
    let root = mutation_root.join(REMOVAL_DIR_NAME);
    for path in [&state_root, &mutation_root, &root] {
        validate_managed_recovery_directory(path)?;
    }
    let digest_dirs = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    let mut required = false;
    for digest_dir in digest_dirs {
        let digest_dir = digest_dir?;
        admit_recovery_entry(&mut count, "managed removal obligations")?;
        if !digest_dir.file_type()?.is_dir() {
            return Err(StateError::InvalidState(
                "managed removal digest path is not a directory".to_string(),
            ));
        }
        validate_reserved_directory_metadata(&fs::symlink_metadata(digest_dir.path())?)?;
        let digest = digest_dir
            .file_name()
            .to_str()
            .filter(|value| {
                value.len() == 128 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| {
                StateError::InvalidState("managed removal digest is invalid".to_string())
            })?
            .to_string();
        required = true;
        for entry in fs::read_dir(digest_dir.path())? {
            let entry = entry?;
            admit_recovery_entry(&mut count, "managed removal obligations")?;
            let filename = entry
                .file_name()
                .to_str()
                .ok_or_else(|| StateError::InvalidFilename("managed removal".to_string()))?
                .to_string();
            validate_managed_filename(&filename)?;
            if !entry.file_type()?.is_file() || !path_matches_sha512(&entry.path(), &digest)? {
                return Err(StateError::InvalidIntegrity {
                    filename,
                    reason: "managed removal obligation ownership cannot be proven".to_string(),
                });
            }
        }
    }
    Ok(required)
}

pub(crate) fn reconcile_managed_removal_obligations(
    instance_mods_dir: &Path,
    current_state: Option<&CompositionState>,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    let state_root = instance_mods_dir.join(STATE_DIR_NAME);
    let mutation_root = state_root.join(MUTATION_DIR_NAME);
    let root = mutation_root.join(REMOVAL_DIR_NAME);
    for path in [&state_root, &mutation_root, &root] {
        validate_managed_recovery_directory(path)?;
    }
    let digest_dirs = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    let mut admitted_digest_dirs = Vec::new();
    for digest_dir in digest_dirs {
        let digest_dir = digest_dir?;
        admit_recovery_entry(&mut count, "managed removal obligations")?;
        if !digest_dir.file_type()?.is_dir() {
            return Err(StateError::InvalidState(
                "managed removal digest path is not a directory".to_string(),
            ));
        }
        let digest = digest_dir
            .file_name()
            .to_str()
            .filter(|value| {
                value.len() == 128 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| {
                StateError::InvalidState("managed removal digest is invalid".to_string())
            })?
            .to_string();
        let digest_path = digest_dir.path();
        for entry in fs::read_dir(&digest_path)? {
            let entry = entry?;
            admit_recovery_entry(&mut count, "managed removal obligations")?;
            let filename = entry
                .file_name()
                .to_str()
                .ok_or_else(|| StateError::InvalidFilename("managed removal".to_string()))?
                .to_string();
            validate_managed_filename(&filename)?;
            if !entry.file_type()?.is_file() || !path_matches_sha512(&entry.path(), &digest)? {
                return Err(StateError::InvalidIntegrity {
                    filename,
                    reason: "managed removal obligation ownership cannot be proven".to_string(),
                });
            }
        }
        admitted_digest_dirs.push((digest_path, digest));
    }

    for (digest_path, digest) in admitted_digest_dirs {
        for entry in fs::read_dir(&digest_path)? {
            let entry = entry?;
            let filename = entry
                .file_name()
                .to_str()
                .ok_or_else(|| StateError::InvalidFilename("managed removal".to_string()))?
                .to_string();
            validate_managed_filename(&filename)?;
            if !entry.file_type()?.is_file() || !path_matches_sha512(&entry.path(), &digest)? {
                return Err(StateError::InvalidIntegrity {
                    filename,
                    reason: "managed removal obligation changed after admission".to_string(),
                });
            }
            let tracked = current_state.is_some_and(|state| {
                state.installed_mods.iter().any(|installed| {
                    installed.filename == filename
                        && installed.integrity.sha512.eq_ignore_ascii_case(&digest)
                })
            });
            let final_path = managed_artifact_path(instance_mods_dir, &filename)?;
            if tracked {
                if !path_exists(&final_path)? {
                    fs::rename(entry.path(), final_path)?;
                } else if path_matches_sha512(&final_path, &digest)? {
                    remove_file_matching_sha512(
                        &entry.path(),
                        &digest,
                        MANAGED_ARTIFACT_MAX_BYTES,
                    )?;
                } else {
                    return Err(StateError::InvalidIntegrity {
                        filename,
                        reason: "managed removal destination conflicts with retained ownership"
                            .to_string(),
                    });
                }
            } else {
                remove_file_matching_sha512(&entry.path(), &digest, MANAGED_ARTIFACT_MAX_BYTES)?;
            }
        }
        settle_empty_removal_digest_directory(&digest_path)?;
    }
    Ok(())
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
        Ok(metadata) if metadata.file_type().is_dir() => {
            validate_reserved_directory_metadata(&metadata)?;
            Ok(true)
        }
        Ok(_) => Err(StateError::InvalidRollback(
            "rollback internal path is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn ensure_managed_directory(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            validate_reserved_directory_metadata(&metadata)
        }
        Ok(_) => Err(StateError::InvalidRollback(
            "rollback internal path is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_reserved_directory(path)?;
            if validate_existing_directory(path)? {
                if let Some(parent) = path.parent() {
                    sync_rollback_directory(parent, RollbackDurabilityPoint::DirectoryCreated)?;
                }
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

#[derive(Debug)]
struct RollbackRenameFailure {
    error: StateError,
    renamed: bool,
}

fn inject_rollback_durability_failure(_point: RollbackDurabilityPoint) -> Result<(), StateError> {
    #[cfg(test)]
    if ROLLBACK_DURABILITY_FAILURE.with(|failure| {
        if failure.get() == Some(_point) {
            failure.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(StateError::Read(std::io::Error::other(format!(
            "injected rollback durability failure at {_point:?}"
        ))));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_rollback_directory(path: &Path, point: RollbackDurabilityPoint) -> Result<(), StateError> {
    inject_rollback_durability_failure(point)?;
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_rollback_directory(_path: &Path, point: RollbackDurabilityPoint) -> Result<(), StateError> {
    inject_rollback_durability_failure(point)
}

fn durable_rollback_rename(
    source: &Path,
    destination: &Path,
    point: RollbackDurabilityPoint,
) -> Result<(), RollbackRenameFailure> {
    let source_parent = source.parent().ok_or_else(|| RollbackRenameFailure {
        error: StateError::InvalidRollback("rollback source parent is invalid".to_string()),
        renamed: false,
    })?;
    let destination_parent = destination.parent().ok_or_else(|| RollbackRenameFailure {
        error: StateError::InvalidRollback("rollback destination parent is invalid".to_string()),
        renamed: false,
    })?;
    let source_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| RollbackRenameFailure {
            error: StateError::InvalidRollback("rollback source name is invalid".to_string()),
            renamed: false,
        })?;
    let destination_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| RollbackRenameFailure {
            error: StateError::InvalidRollback("rollback destination name is invalid".to_string()),
            renamed: false,
        })?;
    let source_anchor =
        AnchoredDirectory::open(source_parent).map_err(|error| RollbackRenameFailure {
            error: StateError::Read(error),
            renamed: false,
        })?;
    let destination_anchor =
        AnchoredDirectory::open(destination_parent).map_err(|error| RollbackRenameFailure {
            error: StateError::Read(error),
            renamed: false,
        })?;
    if let Err(error) =
        source_anchor.rename_file_no_replace(source_name, &destination_anchor, destination_name)
    {
        let renamed = !source.exists() && destination.exists();
        return Err(RollbackRenameFailure {
            error: StateError::Read(error),
            renamed,
        });
    }
    inject_rollback_durability_failure(point).map_err(|error| RollbackRenameFailure {
        error,
        renamed: true,
    })
}

fn validate_rollback_snapshot(snapshot: &RollbackSnapshot) -> Result<(), StateError> {
    if snapshot.schema_version != ROLLBACK_SCHEMA_VERSION {
        return Err(StateError::InvalidRollback(format!(
            "unsupported schema version {}",
            snapshot.schema_version
        )));
    }
    validate_rollback_snapshot_id(&snapshot.id)?;
    if snapshot.created_at.trim() != snapshot.created_at
        || snapshot.created_at.is_empty()
        || snapshot.created_at.chars().count() > STATE_TIMESTAMP_MAX_CHARS
    {
        return Err(StateError::InvalidRollback(
            "rollback timestamp is invalid".to_string(),
        ));
    }
    if let Some(state) = snapshot.state() {
        validate_state(state)?;
    }
    if snapshot.artifacts.len() > STATE_MAX_INSTALLED_MODS {
        return Err(StateError::InvalidRollback(
            "rollback artifact count exceeds the limit".to_string(),
        ));
    }

    let mut artifact_filenames = HashSet::new();
    let mut stored_filenames = HashSet::new();

    for artifact in &snapshot.artifacts {
        validate_managed_filename(&artifact.filename)?;
        validate_managed_filename(&artifact.stored_filename)?;
        validate_state_token("rollback project id", &artifact.project_id)?;
        validate_state_token("rollback version id", &artifact.version_id)?;
        if !artifact_filenames.insert(artifact.filename.to_ascii_lowercase())
            || !stored_filenames.insert(artifact.stored_filename.to_ascii_lowercase())
        {
            return Err(StateError::InvalidRollback(
                "rollback contains duplicate or case-colliding artifact identities".to_string(),
            ));
        }
        let Some(installed) = snapshot
            .state()
            .into_iter()
            .flat_map(|state| state.installed_mods.iter())
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
            || artifact.sha512 != installed.integrity.sha512
        {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} metadata does not match rollback state",
                artifact.filename
            )));
        }
        validate_sha512_integrity(&artifact.filename, &artifact.sha512)?;
    }

    let installed_count = snapshot
        .state()
        .map_or(0, |state| state.installed_mods.len());
    if artifact_filenames.len() != installed_count {
        return Err(StateError::InvalidRollback(
            "rollback artifacts do not cover the complete managed state".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_rollback_snapshot_id(snapshot_id: &str) -> Result<(), StateError> {
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

fn managed_artifacts(state: Option<&CompositionState>) -> HashMap<String, InstalledMod> {
    state
        .map(|state| {
            state
                .installed_mods
                .iter()
                .map(|installed| (installed.filename.clone(), installed.clone()))
                .collect()
        })
        .unwrap_or_default()
}

struct RollbackRestoreTarget {
    source_path: PathBuf,
    temp_path: PathBuf,
    backup_path: PathBuf,
    addition_path: Option<PathBuf>,
    final_path: PathBuf,
    filename: String,
    previous_sha512: Option<String>,
    restored_sha512: String,
}

fn prepare_rollback_restore_targets(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
    current_artifacts: &HashMap<String, InstalledMod>,
) -> Result<Vec<RollbackRestoreTarget>, StateError> {
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
        let previous = current_artifacts.get(&artifact.filename);
        let final_exists = path_exists(&final_path)?;
        if previous.is_none() && final_exists {
            return Err(StateError::InvalidRollback(format!(
                "rollback target {} is not tracked by current managed state",
                artifact.filename
            )));
        }
        if let Some(previous) = previous
            && final_exists
            && !managed_artifact_matches(instance_mods_dir, previous)?
        {
            return Err(StateError::InvalidIntegrity {
                filename: previous.filename.clone(),
                reason: "rollback target bytes do not match the recorded ownership digest"
                    .to_string(),
            });
        }
        let temp_path = rollback_restore_temp_path(instance_mods_dir, &stage_id, index);
        let previous_sha512 = previous
            .filter(|_| final_exists)
            .map(|installed| installed.integrity.sha512.clone());
        let backup_path = previous_sha512.as_ref().map_or_else(
            || PathBuf::from(format!("{}.unused", temp_path.display())),
            |digest| PathBuf::from(format!("{}.previous-{digest}", temp_path.display())),
        );
        targets.push(RollbackRestoreTarget {
            source_path,
            backup_path,
            temp_path,
            addition_path: previous_sha512
                .is_none()
                .then(|| {
                    managed_artifact_addition_path(
                        instance_mods_dir,
                        &artifact.filename,
                        &artifact.sha512,
                    )
                })
                .transpose()?,
            final_path,
            filename: artifact.filename.clone(),
            previous_sha512,
            restored_sha512: artifact.sha512.clone(),
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
    let current = read_state_snapshot_if_present(&lock_file_path(instance_mods_dir))?
        .map(|snapshot| snapshot.snapshot.state);
    let entries = match fs::read_dir(rollback_tmp_dir_path(instance_mods_dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "rollback restore recovery entries")?;
        let metadata = entry.file_type()?;
        if !metadata.is_file() {
            return Err(StateError::InvalidRollback(
                "rollback restore staging contains a non-regular entry".to_string(),
            ));
        }
        let filename = entry
            .file_name()
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| {
                StateError::InvalidRollback(
                    "rollback restore staging filename is invalid".to_string(),
                )
            })?;
        let (stem, backup_digest) = if let Some((stem, digest)) = filename
            .split_once("-restore.tmp.previous-")
            .filter(|(_, digest)| {
                digest.len() == 128 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
            (stem, Some(digest))
        } else if let Some(stem) = filename.strip_suffix("-restore.tmp") {
            (stem, None)
        } else {
            return Err(StateError::InvalidRollback(
                "rollback restore staging contains an unknown obligation".to_string(),
            ));
        };
        let (prefix, raw_index) = stem.rsplit_once('-').ok_or_else(|| {
            StateError::InvalidRollback("rollback restore staging identity is invalid".to_string())
        })?;
        let index = raw_index.parse::<usize>().map_err(|_| {
            StateError::InvalidRollback("rollback restore staging index is invalid".to_string())
        })?;
        let Some(snapshot) = snapshots.iter().find_map(|record| {
            let stage_prefix = format!("restore--{}--", record.snapshot.id);
            prefix
                .starts_with(&stage_prefix)
                .then_some(&record.snapshot)
        }) else {
            return Err(StateError::InvalidRollback(
                "rollback restore staging snapshot is not retained".to_string(),
            ));
        };
        let artifact = snapshot.artifacts.get(index).ok_or_else(|| {
            StateError::InvalidRollback(
                "rollback restore staging index is out of range".to_string(),
            )
        })?;
        if let Some(backup_digest) = backup_digest {
            let Some(previous) = current.as_ref().and_then(|state| {
                state
                    .installed_mods
                    .iter()
                    .find(|installed| installed.filename == artifact.filename)
            }) else {
                return Err(StateError::InvalidRollback(
                    "rollback restore backup has no exact prior ownership record".to_string(),
                ));
            };
            if !path_matches_sha512(&entry.path(), backup_digest)? {
                return Err(StateError::InvalidRollback(
                    "rollback restore backup ownership cannot be proven".to_string(),
                ));
            }
            let final_path = managed_artifact_path(instance_mods_dir, &artifact.filename)?;
            if previous
                .integrity
                .sha512
                .eq_ignore_ascii_case(&artifact.sha512)
            {
                if !path_matches_sha512(&final_path, &artifact.sha512)? {
                    return Err(StateError::InvalidRollback(
                        "committed rollback destination ownership cannot be proven".to_string(),
                    ));
                }
                remove_file_matching_sha512(
                    &entry.path(),
                    backup_digest,
                    MANAGED_ARTIFACT_MAX_BYTES,
                )?;
                continue;
            }
            if !previous
                .integrity
                .sha512
                .eq_ignore_ascii_case(backup_digest)
            {
                return Err(StateError::InvalidRollback(
                    "rollback restore backup does not match current state".to_string(),
                ));
            }
            if path_exists(&final_path)? {
                if path_matches_sha512(&final_path, &previous.integrity.sha512)? {
                    remove_file_matching_sha512(
                        &entry.path(),
                        backup_digest,
                        MANAGED_ARTIFACT_MAX_BYTES,
                    )?;
                    continue;
                }
                if !path_matches_sha512(&final_path, &artifact.sha512)? {
                    return Err(StateError::InvalidRollback(
                        "rollback restore destination ownership cannot be proven".to_string(),
                    ));
                }
                remove_file_matching_sha512(
                    &final_path,
                    &artifact.sha512,
                    MANAGED_ARTIFACT_MAX_BYTES,
                )?;
            }
            fs::rename(entry.path(), final_path)?;
            continue;
        }
        let source = rollback_files_dir_path(instance_mods_dir).join(&artifact.stored_filename);
        if bounded_regular_files_match(&entry.path(), &source)? {
            remove_file_matching_sha512(
                &entry.path(),
                &artifact.sha512,
                MANAGED_ARTIFACT_MAX_BYTES,
            )?;
        }
    }
    Ok(())
}

fn path_matches_sha512(path: &Path, expected_sha512: &str) -> Result<bool, StateError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    };
    if metadata.len() > MANAGED_ARTIFACT_MAX_BYTES {
        return Ok(false);
    }
    let digest = bounded_file_sha512(path, metadata.len())?;
    Ok(!digest.is_empty() && hex::encode(digest).eq_ignore_ascii_case(expected_sha512))
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
    Ok(admit_bounded_file_sha512(path, expected_bytes)?.0)
}

fn admit_bounded_file_sha512(
    path: &Path,
    expected_bytes: u64,
) -> Result<(Vec<u8>, Option<crate::file_identity::FileIdentity>), StateError> {
    if expected_bytes > MANAGED_ARTIFACT_MAX_BYTES {
        return Ok((Vec::new(), None));
    }
    let admitted = match crate::file_identity::admit(path) {
        Ok(admitted) => admitted,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Ok((Vec::new(), None));
        }
        Err(error) => return Err(StateError::Read(error)),
    };
    if admitted.metadata().len() != expected_bytes {
        return Ok((Vec::new(), None));
    }
    let identity = admitted.identity();
    let mut file = admitted.into_file();
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
            return Ok((Vec::new(), None));
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected_bytes {
        return Ok((Vec::new(), None));
    }
    if crate::file_identity::revalidate(path, identity, expected_bytes).is_err() {
        return Ok((Vec::new(), None));
    }
    Ok((hasher.finalize().to_vec(), Some(identity)))
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
    let Some(first) = targets.first() else {
        return Ok(());
    };
    let instance_mods_dir = first
        .final_path
        .parent()
        .ok_or_else(|| StateError::InvalidRollback("invalid rollback target".to_string()))?;
    ensure_rollback_internal_roots(instance_mods_dir)?;
    let mut staged = Vec::new();
    for target in targets {
        let result = copy_regular_file_exclusive(&target.source_path, &target.temp_path);
        if let Err(error) = result {
            cleanup_created_rollback_artifacts(&staged)?;
            return Err(error);
        }
        let metadata = fs::symlink_metadata(&target.temp_path)?;
        let digest = bounded_file_sha512(&target.temp_path, metadata.len())?;
        if digest.is_empty() || !hex::encode(digest).eq_ignore_ascii_case(&target.restored_sha512) {
            cleanup_created_rollback_artifacts(&staged)?;
            remove_owned_file(&target.temp_path)?;
            return Err(StateError::InvalidIntegrity {
                filename: target.filename.clone(),
                reason: "staged rollback bytes do not match the recorded ownership digest"
                    .to_string(),
            });
        }
        if let Some(expected_path) = &target.addition_path {
            let staged_addition = stage_managed_artifact_addition(
                instance_mods_dir,
                &target.filename,
                &target.restored_sha512,
                &target.temp_path,
            )?;
            if staged_addition != *expected_path {
                return Err(StateError::InvalidRollback(
                    "rollback addition obligation path changed during staging".to_string(),
                ));
            }
        }
        staged.push(target.temp_path.as_path());
    }
    Ok(())
}

fn publish_rollback_restore_target(target: &RollbackRestoreTarget) -> Result<(), StateError> {
    if let Some(previous_sha512) = target.previous_sha512.as_deref() {
        if !path_matches_sha512(&target.final_path, previous_sha512)? {
            return Err(StateError::InvalidIntegrity {
                filename: target.filename.clone(),
                reason: "rollback target changed before publication".to_string(),
            });
        }
        reserve_backup_exclusive(
            &target.final_path,
            &target.backup_path,
            StatePublicationPhase::Backup,
            Some(previous_sha512),
        )?;
        let backup_metadata = fs::symlink_metadata(&target.backup_path)?;
        let backup_digest = bounded_file_sha512(&target.backup_path, backup_metadata.len())?;
        if backup_digest.is_empty()
            || !hex::encode(backup_digest).eq_ignore_ascii_case(previous_sha512)
        {
            return match fs::rename(&target.backup_path, &target.final_path) {
                Ok(()) => Err(StateError::InvalidIntegrity {
                    filename: target.filename.clone(),
                    reason: "rollback target changed during publication".to_string(),
                }),
                Err(error) => Err(StateError::Read(error)),
            };
        }
    }
    let publication = if let Some(addition_path) = &target.addition_path {
        fs::hard_link(addition_path, &target.final_path)
    } else {
        fs::rename(&target.temp_path, &target.final_path)
    };
    if let Err(error) = publication {
        if path_exists(&target.backup_path)? && !path_exists(&target.final_path)? {
            fs::rename(&target.backup_path, &target.final_path)?;
        }
        return Err(StateError::Read(error));
    }
    if let Some(addition_path) = &target.addition_path {
        let addition = admit_file_identity(addition_path).map_err(|error| {
            identity_admission_error(
                error,
                StateError::InvalidRollback(
                    "rollback addition publication ownership cannot be proven".to_string(),
                ),
            )
        })?;
        if crate::file_identity::revalidate(&target.final_path, addition.0, addition.1).is_err()
            || !path_matches_sha512(&target.final_path, &target.restored_sha512)?
        {
            return Err(StateError::InvalidRollback(
                "rollback addition publication ownership cannot be proven".to_string(),
            ));
        }
    }
    Ok(())
}

fn cleanup_rollback_restore_backups(targets: &[RollbackRestoreTarget]) -> Result<(), StateError> {
    for target in targets {
        if path_exists(&target.backup_path)? {
            remove_owned_file(&target.backup_path)?;
        }
    }
    Ok(())
}

fn compensate_rollback_restore_targets(
    targets: &[RollbackRestoreTarget],
) -> Result<(), StateError> {
    for target in targets.iter().rev() {
        if path_exists(&target.backup_path)? {
            if path_exists(&target.final_path)? {
                if !path_matches_sha512(&target.final_path, &target.restored_sha512)? {
                    return Err(StateError::InvalidRollback(
                        "rollback compensation destination ownership cannot be proven".to_string(),
                    ));
                }
                remove_owned_file(&target.final_path)?;
            }
            fs::rename(&target.backup_path, &target.final_path)?;
        } else if let Some(addition_path) = &target.addition_path
            && path_exists(&target.final_path)?
        {
            let addition = admit_file_identity(addition_path).map_err(|error| {
                identity_admission_error(
                    error,
                    StateError::InvalidRollback(
                        "rollback compensation created target ownership cannot be proven"
                            .to_string(),
                    ),
                )
            })?;
            if crate::file_identity::revalidate(&target.final_path, addition.0, addition.1).is_err()
                || !path_matches_sha512(&target.final_path, &target.restored_sha512)?
            {
                return Err(StateError::InvalidRollback(
                    "rollback compensation created target ownership cannot be proven".to_string(),
                ));
            }
            remove_identity_bound_file(
                &target.final_path,
                addition.0,
                addition.1,
                &target.restored_sha512,
            )?;
        }
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
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => {
            return Err(StateError::InvalidState(
                "managed cleanup target is not a regular file".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let (admitted, digest) = admit_live_cleanup_file(path, metadata.len())?;
    quarantine_remove_admitted_file(path, admitted, &digest, |_| {})
}

fn remove_file_matching_sha512(
    path: &Path,
    expected_sha512: &str,
    max_bytes: u64,
) -> Result<(), StateError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= max_bytes => metadata,
        Ok(_) => {
            return Err(StateError::InvalidState(
                "managed cleanup target is not a bounded regular file".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let (admitted, digest) = admit_live_cleanup_file(path, metadata.len())?;
    if !digest.eq_ignore_ascii_case(expected_sha512) {
        return Err(StateError::InvalidState(
            "managed cleanup target bytes changed after admission".to_string(),
        ));
    }
    quarantine_remove_admitted_file(path, admitted, expected_sha512, |_| {})
}

fn remove_identity_bound_file(
    path: &Path,
    admitted: crate::file_identity::FileIdentity,
    admitted_len: u64,
    expected_sha512: &str,
) -> Result<(), StateError> {
    let (settlement, digest) = admit_live_cleanup_file(path, admitted_len)?;
    if settlement.identity() != admitted || !digest.eq_ignore_ascii_case(expected_sha512) {
        return Err(StateError::InvalidState(
            "managed cleanup target identity or digest changed after admission".to_string(),
        ));
    }
    quarantine_remove_admitted_file(path, settlement, expected_sha512, |_| {})
}

fn admit_file_identity(
    path: &Path,
) -> Result<(crate::file_identity::FileIdentity, u64), std::io::Error> {
    let admitted = crate::file_identity::admit(path)?;
    Ok((admitted.identity(), admitted.metadata().len()))
}

fn identity_admission_error(error: std::io::Error, invalid: StateError) -> StateError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        invalid
    } else {
        StateError::Read(error)
    }
}

#[cfg(test)]
fn quarantine_remove_file_with_hook(
    path: &Path,
    expected_sha512: &str,
    max_bytes: u64,
    after_park: impl FnOnce(&Path),
) -> Result<(), StateError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(StateError::InvalidState(
            "managed cleanup target is not a bounded regular file".to_string(),
        ));
    }
    let (admitted, digest) = admit_live_cleanup_file(path, metadata.len())?;
    if !digest.eq_ignore_ascii_case(expected_sha512) {
        return Err(StateError::InvalidState(
            "managed cleanup target digest changed after admission".to_string(),
        ));
    }
    quarantine_remove_admitted_file_inner(
        path,
        admitted,
        expected_sha512,
        |_| {},
        after_park,
        |_| {},
    )
}

#[cfg(test)]
fn quarantine_remove_file_with_parking_hook(
    path: &Path,
    expected_sha512: &str,
    max_bytes: u64,
    before_park: impl FnOnce(&Path),
) -> Result<(), StateError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(StateError::InvalidState(
            "managed cleanup target is not a bounded regular file".to_string(),
        ));
    }
    let (admitted, digest) = admit_live_cleanup_file(path, metadata.len())?;
    if !digest.eq_ignore_ascii_case(expected_sha512) {
        return Err(StateError::InvalidState(
            "managed cleanup target digest changed after admission".to_string(),
        ));
    }
    quarantine_remove_admitted_file_inner(
        path,
        admitted,
        expected_sha512,
        before_park,
        |_| {},
        |_| {},
    )
}

fn quarantine_remove_admitted_file(
    path: &Path,
    admitted: crate::file_identity::AdmittedFile,
    expected_sha512: &str,
    after_park: impl FnOnce(&Path),
) -> Result<(), StateError> {
    quarantine_remove_admitted_file_inner(
        path,
        admitted,
        expected_sha512,
        |_| {},
        after_park,
        |_| {},
    )
}

#[cfg(test)]
fn quarantine_remove_file_with_settlement_hook(
    path: &Path,
    expected_sha512: &str,
    max_bytes: u64,
    before_settlement: impl FnOnce(&Path),
) -> Result<(), StateError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(StateError::InvalidState(
            "managed cleanup target is not a bounded regular file".to_string(),
        ));
    }
    let (admitted, digest) = admit_live_cleanup_file(path, metadata.len())?;
    if !digest.eq_ignore_ascii_case(expected_sha512) {
        return Err(StateError::InvalidState(
            "managed cleanup target digest changed after admission".to_string(),
        ));
    }
    quarantine_remove_admitted_file_inner(
        path,
        admitted,
        expected_sha512,
        |_| {},
        |_| {},
        before_settlement,
    )
}

fn quarantine_remove_admitted_file_inner(
    path: &Path,
    settlement: crate::file_identity::AdmittedFile,
    expected_sha512: &str,
    before_park: impl FnOnce(&Path),
    after_park: impl FnOnce(&Path),
    before_settlement: impl FnOnce(&Path),
) -> Result<(), StateError> {
    if !is_valid_sha512(expected_sha512) {
        return Err(StateError::InvalidState(
            "managed cleanup digest is invalid".to_string(),
        ));
    }
    let admitted = settlement.identity();
    let admitted_len = settlement.metadata().len();
    let admitted_file = settlement.try_clone_file()?;
    let instance_mods_dir = managed_cleanup_root(path)?;
    let quarantine = ReservedCleanupQuarantine::admit_empty(&instance_mods_dir)?;
    crate::file_identity::revalidate(path, admitted, admitted_len).map_err(|_| {
        StateError::InvalidState(
            "managed cleanup target identity changed before parking".to_string(),
        )
    })?;
    let mut nonce = [0_u8; 16];
    OsRng.try_fill_bytes(&mut nonce).map_err(|_| {
        StateError::Read(std::io::Error::other(
            "managed cleanup quarantine nonce generation failed",
        ))
    })?;
    let parked_name = format!(
        "{}.{}.park",
        expected_sha512.to_ascii_lowercase(),
        hex::encode(nonce)
    );
    let parked = quarantine.path(&parked_name);
    before_park(&parked);
    let source_parent = path.parent().ok_or_else(|| {
        StateError::InvalidState("managed cleanup target has no parent".to_string())
    })?;
    let source_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StateError::InvalidState("managed cleanup target name is invalid".to_string())
        })?;
    let source = AnchoredDirectory::open(&source_parent.join("."))?;
    let mut receipt = match source.rename_admitted_file_no_replace(
        source_name,
        &quarantine.directory,
        &parked_name,
        &admitted_file,
    ) {
        axial_minecraft::managed_path::AnchoredFileMoveOutcome::PreMove(error)
            if error.kind() == std::io::ErrorKind::InvalidData =>
        {
            return Err(StateError::InvalidState(error.to_string()));
        }
        axial_minecraft::managed_path::AnchoredFileMoveOutcome::PreMove(error) => {
            return Err(StateError::Read(error));
        }
        axial_minecraft::managed_path::AnchoredFileMoveOutcome::Applied(receipt) => receipt,
        axial_minecraft::managed_path::AnchoredFileMoveOutcome::Indeterminate(error) => {
            return Err(StateError::Read(error));
        }
    };
    if receipt.requires_resync() {
        receipt.resync()?;
    }
    if !receipt.is_exact_admitted_move() {
        return match receipt.restore_create_only() {
            axial_minecraft::managed_path::AnchoredFileRestoreOutcome::Restored => {
                Err(StateError::InvalidState(
                    "managed cleanup live source changed during parking".to_string(),
                ))
            }
            axial_minecraft::managed_path::AnchoredFileRestoreOutcome::SourceOccupied => {
                Err(StateError::InvalidState(
                    "managed cleanup live replacement could not be restored create-only"
                        .to_string(),
                ))
            }
            axial_minecraft::managed_path::AnchoredFileRestoreOutcome::Indeterminate(error) => {
                Err(StateError::Read(error))
            }
        };
    }
    drop(receipt);
    after_park(&parked);
    let parked_matches = crate::file_identity::revalidate(&parked, admitted, admitted_len).is_ok()
        && path_matches_sha512(&parked, expected_sha512)?;
    if !parked_matches {
        return Err(StateError::InvalidState(
            "managed cleanup parked identity changed after admission".to_string(),
        ));
    }
    before_settlement(&parked);
    match settlement.settle_exact(&parked)? {
        #[cfg(windows)]
        crate::file_identity::ExactFileSettlement::Settled => {
            quarantine.directory.prove_empty().map_err(|_| {
                StateError::InvalidState(
                    "managed cleanup quarantine residue remains after settlement".to_string(),
                )
            })?;
            Ok(())
        }
        #[cfg(unix)]
        crate::file_identity::ExactFileSettlement::IdentityRetained(retained) => {
            quarantine.remove_proven_park(&parked, retained, expected_sha512)
        }
        #[cfg(unix)]
        crate::file_identity::ExactFileSettlement::PathChanged => Err(StateError::InvalidState(
            "managed cleanup quarantine identity changed before settlement".to_string(),
        )),
    }
}

fn managed_cleanup_root(path: &Path) -> Result<PathBuf, StateError> {
    let parent = path.parent().ok_or_else(|| {
        StateError::InvalidState("managed cleanup target has no parent".to_string())
    })?;
    for ancestor in parent.ancestors() {
        if ancestor
            .file_name()
            .is_some_and(|name| name == STATE_DIR_NAME)
        {
            return ancestor.parent().map(Path::to_path_buf).ok_or_else(|| {
                StateError::InvalidState("managed cleanup root is invalid".to_string())
            });
        }
    }
    Ok(parent.to_path_buf())
}

fn cleanup_quarantine_path(instance_mods_dir: &Path) -> PathBuf {
    instance_mods_dir
        .join(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(QUARANTINE_DIR_NAME)
}

fn ensure_cleanup_quarantine(instance_mods_dir: &Path) -> Result<(), StateError> {
    for path in [
        instance_mods_dir.join(STATE_DIR_NAME),
        instance_mods_dir
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME),
        cleanup_quarantine_path(instance_mods_dir),
    ] {
        ensure_recovery_directory(&path)?;
    }
    Ok(())
}

struct ReservedCleanupQuarantine {
    directory: AnchoredDirectory,
}

impl ReservedCleanupQuarantine {
    fn admit_empty(instance_mods_dir: &Path) -> Result<Self, StateError> {
        require_cleanup_quarantine_empty(instance_mods_dir)?;
        ensure_cleanup_quarantine(instance_mods_dir)?;
        require_cleanup_quarantine_empty(instance_mods_dir)?;
        Ok(Self {
            directory: AnchoredDirectory::open(&cleanup_quarantine_path(instance_mods_dir))?,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    #[cfg(unix)]
    fn remove_proven_park(
        &self,
        parked: &Path,
        retained: crate::file_identity::AdmittedFile,
        expected_sha512: &str,
    ) -> Result<(), StateError> {
        let admitted = retained.identity();
        let admitted_len = retained.metadata().len();
        if parked.parent() != Some(self.directory.path())
            || crate::file_identity::revalidate(parked, admitted, admitted_len).is_err()
            || !path_matches_sha512(parked, expected_sha512)?
        {
            return Err(StateError::InvalidState(
                "managed cleanup quarantine proof changed before settlement".to_string(),
            ));
        }
        fs::remove_file(parked)?;
        self.directory.sync()?;
        self.directory.prove_empty().map_err(|_| {
            StateError::InvalidState(
                "managed cleanup quarantine residue remains after settlement".to_string(),
            )
        })?;
        drop(retained);
        Ok(())
    }
}

fn cleanup_quarantine_reconciliation_required(
    instance_mods_dir: &Path,
) -> Result<bool, StateError> {
    Ok(inspect_cleanup_quarantine(instance_mods_dir)? != 0)
}

fn reconcile_cleanup_quarantine(instance_mods_dir: &Path) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)
}

pub(crate) fn require_cleanup_quarantine_empty(instance_mods_dir: &Path) -> Result<(), StateError> {
    if inspect_cleanup_quarantine(instance_mods_dir)? != 0 {
        return Err(StateError::InvalidState(
            "managed cleanup quarantine contains retained same-process obligations".to_string(),
        ));
    }
    Ok(())
}

fn inspect_cleanup_quarantine(instance_mods_dir: &Path) -> Result<usize, StateError> {
    let root = cleanup_quarantine_path(instance_mods_dir);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            validate_reserved_directory_metadata(&metadata)?;
        }
        Ok(_) => {
            return Err(StateError::InvalidState(
                "managed cleanup quarantine is not a regular directory".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(StateError::Read(error)),
    }
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    let mut bytes = 0_u64;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "managed cleanup quarantine")?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.len() > MANAGED_ARTIFACT_MAX_BYTES {
            return Err(StateError::InvalidState(
                "managed cleanup quarantine contains an invalid entry".to_string(),
            ));
        }
        bytes = bytes.checked_add(metadata.len()).ok_or_else(|| {
            StateError::InvalidState("managed cleanup quarantine size overflowed".to_string())
        })?;
        if bytes > CLEANUP_QUARANTINE_MAX_BYTES {
            return Err(StateError::InvalidState(
                "managed cleanup quarantine exceeds its byte budget".to_string(),
            ));
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            StateError::InvalidState("managed cleanup quarantine name is invalid".to_string())
        })?;
        let (digest, suffix) = name.split_once('.').ok_or_else(|| {
            StateError::InvalidState("managed cleanup quarantine name is invalid".to_string())
        })?;
        let nonce = suffix.strip_suffix(".park").ok_or_else(|| {
            StateError::InvalidState("managed cleanup quarantine name is invalid".to_string())
        })?;
        if digest.len() != 128
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            || nonce.len() != 32
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(StateError::InvalidState(
                "managed cleanup quarantine name is not canonical".to_string(),
            ));
        }
        let admitted_entry = crate::file_identity::admit(&path)?;
        if admitted_entry.metadata().len() != metadata.len()
            || crate::file_identity::revalidate(&path, admitted_entry.identity(), metadata.len())
                .is_err()
        {
            return Err(StateError::InvalidState(
                "managed cleanup quarantine entry changed during inspection".to_string(),
            ));
        }
    }
    Ok(count)
}

fn admit_live_cleanup_file(
    path: &Path,
    expected_len: u64,
) -> Result<(crate::file_identity::AdmittedFile, String), StateError> {
    let admitted = crate::file_identity::admit_for_settlement(path)?;
    if admitted.metadata().len() != expected_len || expected_len > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(StateError::InvalidState(
            "managed cleanup target size changed".to_string(),
        ));
    }
    let mut file = admitted.try_clone_file()?;
    let mut hasher = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > expected_len || total > MANAGED_ARTIFACT_MAX_BYTES {
            return Err(StateError::InvalidState(
                "managed cleanup target size changed".to_string(),
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected_len
        || crate::file_identity::revalidate(path, admitted.identity(), expected_len).is_err()
    {
        return Err(StateError::InvalidState(
            "managed cleanup target identity changed during digest admission".to_string(),
        ));
    }
    Ok((admitted, hex::encode(hasher.finalize())))
}

fn cleanup_rollback_restore_targets(targets: &[RollbackRestoreTarget]) -> Result<(), StateError> {
    for target in targets {
        remove_owned_file(&target.temp_path)?;
    }
    Ok(())
}

fn validate_state(state: &CompositionState) -> Result<(), StateError> {
    crate::install::plan::validate_state_graph(state)
        .map_err(|error| StateError::InvalidState(error.to_string()))?;
    if state.installed_at.trim() != state.installed_at
        || state.installed_at.is_empty()
        || state.installed_at.chars().count() > STATE_TIMESTAMP_MAX_CHARS
    {
        return Err(StateError::InvalidState(
            "performance state timestamp is invalid".to_string(),
        ));
    }
    if state.installed_mods.len() > STATE_MAX_INSTALLED_MODS {
        return Err(StateError::InvalidState(
            "performance state installed artifact count exceeds the limit".to_string(),
        ));
    }
    let mut project_ids = HashSet::new();
    let mut filenames = HashSet::new();
    for installed in &state.installed_mods {
        validate_managed_filename(&installed.filename)?;
        validate_state_token("managed project id", &installed.project_id)?;
        validate_state_token("managed version id", &installed.version_id)?;
        if !project_ids.insert(installed.project_id.to_ascii_lowercase()) {
            return Err(StateError::InvalidState(
                "performance state contains duplicate or case-colliding project ids".to_string(),
            ));
        }
        if !filenames.insert(installed.filename.to_ascii_lowercase()) {
            return Err(StateError::InvalidState(
                "performance state contains duplicate or case-colliding filenames".to_string(),
            ));
        }
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
    }
    Ok(())
}

fn validate_persisted_state(snapshot: &PersistedCompositionState) -> Result<(), StateError> {
    if snapshot.schema_version != STATE_SCHEMA_VERSION {
        return Err(StateError::InvalidState(format!(
            "unsupported performance state schema version {}",
            snapshot.schema_version
        )));
    }
    validate_state(&snapshot.state)
}

fn validate_state_token(label: &str, value: &str) -> Result<(), StateError> {
    if value.trim() != value
        || value.is_empty()
        || value.chars().count() > STATE_TOKEN_MAX_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(StateError::InvalidState(format!("{label} is invalid")));
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

fn is_valid_sha512(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn admit_recovery_entry(count: &mut usize, label: &str) -> Result<(), StateError> {
    *count = count.saturating_add(1);
    if *count > RECOVERY_ENTRY_LIMIT {
        Err(StateError::InvalidState(format!(
            "{label} exceed the recovery entry limit"
        )))
    } else {
        Ok(())
    }
}

fn ensure_recovery_directory(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            validate_reserved_directory_metadata(&metadata)
        }
        Ok(_) => Err(StateError::InvalidState(
            "managed recovery path is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_reserved_directory(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(StateError::Read(error)),
            }
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    validate_reserved_directory_metadata(&metadata)?;
                    if let Some(parent) = path.parent() {
                        sync_rollback_directory(parent, RollbackDurabilityPoint::DirectoryCreated)?;
                    }
                    Ok(())
                }
                Ok(_) => Err(StateError::InvalidState(
                    "managed recovery directory was not created safely".to_string(),
                )),
                Err(error) => Err(StateError::Read(error)),
            }
        }
        Err(error) => Err(StateError::Read(error)),
    }
}

fn create_reserved_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn validate_reserved_directory_metadata(_metadata: &fs::Metadata) -> Result<(), StateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if _metadata.permissions().mode() & 0o077 != 0 {
            return Err(StateError::InvalidState(
                "managed internal directory is not owner-only".to_string(),
            ));
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
        || trimmed.len() > STATE_FILENAME_MAX_BYTES
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

fn cleanup_proven_history_temps(instance_mods_dir: &Path) -> Result<(), StateError> {
    let history_dir = rollback_history_dir_path(instance_mods_dir);
    let entries = match fs::read_dir(&history_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "rollback history recovery entries")?;
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
                remove_rollback_history_candidate(instance_mods_dir, &temp_path)?;
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
            return Err(StateError::InvalidRollback(
                "latest rollback metadata has no canonical history record".to_string(),
            ));
        }
        cleanup_abandoned_snapshot_artifacts(instance_mods_dir, &snapshot)?;
        sync_rollback_directory(
            &rollback_files_dir_path(instance_mods_dir),
            RollbackDurabilityPoint::ArtifactCopiesCleaned,
        )?;
        remove_rollback_history_candidate(instance_mods_dir, &temp_path)?;
    }
    Ok(())
}

fn prove_managed_internal_roots(instance_mods_dir: &Path) -> Result<(), StateError> {
    let state_root = instance_mods_dir.join(STATE_DIR_NAME);
    validate_managed_recovery_directory(&state_root)?;
    let entries = match fs::read_dir(&state_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "managed internal root entries")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            StateError::InvalidState("managed internal root name is invalid".to_string())
        })?;
        if !matches!(name, MUTATION_DIR_NAME | ROLLBACK_DIR_NAME) || !entry.file_type()?.is_dir() {
            return Err(StateError::InvalidState(
                "managed internal root contains an unknown entry".to_string(),
            ));
        }
        validate_reserved_directory_metadata(&fs::symlink_metadata(entry.path())?)?;
    }

    let mutation_root = state_root.join(MUTATION_DIR_NAME);
    validate_managed_recovery_directory(&mutation_root)?;
    let entries = match fs::read_dir(&mutation_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "managed internal root entries")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            StateError::InvalidState("managed mutation root name is invalid".to_string())
        })?;
        let known = matches!(
            name,
            REMOVAL_DIR_NAME | ADDITION_DIR_NAME | QUARANTINE_DIR_NAME
        );
        if !known || !entry.file_type()?.is_dir() {
            return Err(StateError::InvalidState(
                "managed mutation root contains an unknown entry".to_string(),
            ));
        }
        validate_reserved_directory_metadata(&fs::symlink_metadata(entry.path())?)?;
    }
    if path_exists(&mutation_root.join(ADDITION_DIR_NAME))? {
        return Err(StateError::InvalidState(
            "managed addition obligation root remains after recovery".to_string(),
        ));
    }
    require_cleanup_quarantine_empty(instance_mods_dir)
}

fn prove_removal_obligations_settled(instance_mods_dir: &Path) -> Result<(), StateError> {
    let root = instance_mods_dir
        .join(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(REMOVAL_DIR_NAME);
    validate_managed_recovery_directory(&root)?;
    let digest_dirs = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for digest_dir in digest_dirs {
        let digest_dir = digest_dir?;
        admit_recovery_entry(&mut count, "managed removal proof entries")?;
        if !digest_dir.file_type()?.is_dir()
            || digest_dir
                .file_name()
                .to_str()
                .is_none_or(|value| !is_valid_sha512(value))
        {
            return Err(StateError::InvalidState(
                "managed removal root contains an invalid entry".to_string(),
            ));
        }
        return Err(StateError::InvalidState(
            "managed removal obligation directory remains after recovery".to_string(),
        ));
    }
    Ok(())
}

fn validate_managed_recovery_directory(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            validate_reserved_directory_metadata(&metadata)
        }
        Ok(_) => Err(StateError::InvalidState(
            "managed recovery root is not a regular directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn prove_rollback_storage_settled(instance_mods_dir: &Path) -> Result<(), StateError> {
    validate_rollback_internal_roots(instance_mods_dir)?;
    let rollback_root = rollback_dir_path(instance_mods_dir);
    let entries = match fs::read_dir(&rollback_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "rollback proof root entries")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            StateError::InvalidRollback("rollback root name is invalid".to_string())
        })?;
        let valid = match name {
            ROLLBACK_FILE_NAME => entry.file_type()?.is_file(),
            ROLLBACK_FILES_DIR_NAME | ROLLBACK_HISTORY_DIR_NAME | ROLLBACK_TMP_DIR_NAME => {
                entry.file_type()?.is_dir()
            }
            _ => false,
        };
        if !valid {
            return Err(StateError::InvalidRollback(
                "rollback root contains an unknown or transient entry".to_string(),
            ));
        }
    }

    let snapshots = load_retained_rollback_snapshots(instance_mods_dir)?;
    if let Some(latest) = snapshots.iter().find(|record| record.latest) {
        let latest_path = rollback_file_path(instance_mods_dir);
        let history_path = rollback_history_file_path(instance_mods_dir, &latest.snapshot.id);
        let latest_metadata = fs::symlink_metadata(&latest_path)?;
        let history_metadata = fs::symlink_metadata(&history_path)?;
        if !latest_metadata.file_type().is_file()
            || !history_metadata.file_type().is_file()
            || !bounded_regular_files_match(&latest_path, &history_path)?
        {
            return Err(StateError::InvalidRollback(
                "latest rollback metadata does not exactly match its history publication"
                    .to_string(),
            ));
        }
    }
    let mut retained_artifacts = HashMap::new();
    for record in &snapshots {
        for (index, artifact) in record.snapshot.artifacts.iter().enumerate() {
            if artifact.stored_filename != format!("{}-{index}.bin", record.snapshot.id)
                || retained_artifacts
                    .insert(artifact.stored_filename.as_str(), artifact.sha512.as_str())
                    .is_some()
            {
                return Err(StateError::InvalidRollback(
                    "retained rollback artifact identity is invalid or ambiguous".to_string(),
                ));
            }
        }
    }
    for (stored_filename, expected) in &retained_artifacts {
        let path = rollback_files_dir_path(instance_mods_dir).join(stored_filename);
        if !path_matches_sha512(&path, expected)? {
            return Err(StateError::InvalidRollback(
                "retained rollback artifact ownership cannot be proven".to_string(),
            ));
        }
    }
    prove_rollback_directory_files(
        &rollback_history_dir_path(instance_mods_dir),
        |name, path| {
            let snapshot_id = name.strip_suffix(".json").ok_or_else(|| {
                StateError::InvalidRollback(
                    "rollback history contains an unknown or transient entry".to_string(),
                )
            })?;
            validate_rollback_snapshot_id(snapshot_id)?;
            let snapshot = read_rollback_snapshot_file(path)?;
            if snapshot.id != snapshot_id {
                return Err(StateError::InvalidRollback(
                    "rollback history filename does not match its payload id".to_string(),
                ));
            }
            Ok(())
        },
    )?;
    prove_rollback_directory_files(&rollback_files_dir_path(instance_mods_dir), |name, path| {
        let expected = retained_artifacts.get(name).ok_or_else(|| {
            StateError::InvalidRollback("rollback artifact is not retained by metadata".to_string())
        })?;
        if !path_matches_sha512(path, expected)? {
            return Err(StateError::InvalidRollback(
                "retained rollback artifact ownership cannot be proven".to_string(),
            ));
        }
        Ok(())
    })?;
    let tmp = rollback_tmp_dir_path(instance_mods_dir);
    match fs::read_dir(tmp) {
        Ok(mut entries) => {
            if let Some(entry) = entries.next() {
                entry?;
                return Err(StateError::InvalidRollback(
                    "rollback staging obligation remains after recovery".to_string(),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn prove_rollback_directory_files(
    directory: &Path,
    mut prove: impl FnMut(&str, &Path) -> Result<(), StateError>,
) -> Result<(), StateError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "rollback proof directory entries")?;
        if !entry.file_type()?.is_file() {
            return Err(StateError::InvalidRollback(
                "rollback storage contains a non-regular entry".to_string(),
            ));
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            StateError::InvalidRollback("rollback storage name is invalid".to_string())
        })?;
        prove(name, &entry.path())?;
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
        let path = rollback_files_dir_path(instance_mods_dir).join(&artifact.stored_filename);
        let staged = rollback_files_dir_path(instance_mods_dir)
            .join(format!("{}.new.tmp", artifact.stored_filename));
        if path_exists(&path)? {
            if !path_matches_sha512(&path, &artifact.sha512)? {
                return Err(StateError::InvalidRollback(
                    "abandoned rollback artifact ownership cannot be proven".to_string(),
                ));
            }
            remove_file_matching_sha512(&path, &artifact.sha512, MANAGED_ARTIFACT_MAX_BYTES)?;
        }
        if path_exists(&staged)? {
            let metadata = fs::symlink_metadata(&staged)?;
            if !metadata.file_type().is_file() || metadata.len() > MANAGED_ARTIFACT_MAX_BYTES {
                return Err(StateError::InvalidRollback(
                    "abandoned rollback staging ownership cannot be proven".to_string(),
                ));
            }
            remove_owned_file(&staged)?;
        }
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
    let history_path = rollback_history_file_path(instance_mods_dir, &temp_snapshot.id);
    let matches_history = match fs::symlink_metadata(&history_path) {
        Ok(_) => temp_data == read_bounded_regular_metadata_file(&history_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(StateError::Read(error)),
    };
    let candidate_artifacts_absent = temp_snapshot.artifacts.iter().all(|artifact| {
        !rollback_files_dir_path(instance_mods_dir)
            .join(&artifact.stored_filename)
            .exists()
    });
    if !matches_latest && !matches_history && !candidate_artifacts_absent {
        return Err(StateError::InvalidRollback(
            "latest rollback temp ownership cannot be proven".to_string(),
        ));
    }
    remove_owned_file(&temp_path)
}

fn read_bounded_regular_metadata_file(path: &Path) -> Result<Vec<u8>, StateError> {
    let admitted = crate::file_identity::admit(path).map_err(|error| {
        identity_admission_error(
            error,
            StateError::InvalidRollback(
                "rollback metadata obligation is not a bounded regular file".to_string(),
            ),
        )
    })?;
    if admitted.metadata().len() > ROLLBACK_METADATA_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback metadata obligation is not a bounded regular file".to_string(),
        ));
    }
    let identity = admitted.identity();
    let admitted_len = admitted.metadata().len();
    let mut file = admitted.into_file();
    let mut data = Vec::with_capacity(admitted_len as usize);
    std::io::Read::by_ref(&mut file)
        .take(ROLLBACK_METADATA_MAX_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 != admitted_len
        || crate::file_identity::revalidate(path, identity, admitted_len).is_err()
    {
        return Err(StateError::InvalidRollback(
            "rollback metadata changed while reconciling cleanup".to_string(),
        ));
    }
    Ok(data)
}

fn read_rollback_snapshot_file(path: &Path) -> Result<RollbackSnapshot, StateError> {
    let data = read_bounded_regular_metadata_file(path)?;
    let snapshot = serde_json::from_slice::<RollbackSnapshot>(&data)?;
    validate_rollback_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn write_rollback_snapshot(
    path: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<(), RollbackMetadataPublicationFailure> {
    let data = serde_json::to_vec_pretty(snapshot)
        .map_err(StateError::from)
        .map_err(RollbackMetadataPublicationFailure::PrePublish)?;
    if data.len() as u64 > ROLLBACK_METADATA_MAX_BYTES {
        return Err(RollbackMetadataPublicationFailure::PrePublish(
            StateError::InvalidRollback("rollback metadata exceeds the byte budget".to_string()),
        ));
    }
    reconcile_rollback_metadata_publication_for_write(path)
        .map_err(RollbackMetadataPublicationFailure::PrePublish)?;
    let temp_path = path.with_extension("json.tmp");
    let mut temp = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(StateError::Read)
        .map_err(RollbackMetadataPublicationFailure::PrePublish)?;
    if let Err(error) = temp.write_all(&data).and_then(|()| temp.sync_all()) {
        drop(temp);
        remove_owned_file(&temp_path).map_err(RollbackMetadataPublicationFailure::PrePublish)?;
        return Err(RollbackMetadataPublicationFailure::PrePublish(
            StateError::Read(error),
        ));
    }
    drop(temp);
    let parent = path.parent().ok_or_else(|| {
        RollbackMetadataPublicationFailure::PrePublish(StateError::InvalidRollback(
            "rollback metadata parent is invalid".to_string(),
        ))
    })?;
    if let Err(error) = sync_rollback_directory(parent, RollbackDurabilityPoint::LatestStaged) {
        remove_owned_file(&temp_path).map_err(RollbackMetadataPublicationFailure::PrePublish)?;
        return Err(RollbackMetadataPublicationFailure::PrePublish(error));
    }
    publish_rollback_metadata(&temp_path, path)
}

fn rollback_metadata_backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.previous.tmp")
}

fn rollback_publication_reconciliation_required(
    instance_mods_dir: &Path,
) -> Result<bool, StateError> {
    validate_rollback_internal_roots(instance_mods_dir)?;
    let path = rollback_file_path(instance_mods_dir);
    let backup = rollback_metadata_backup_path(&path);
    match fs::symlink_metadata(&backup) {
        Ok(_) => {
            admitted_rollback_file_sha512(&backup)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StateError::Read(error)),
    }
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            admitted_rollback_file_sha512(&path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(StateError::Read(error)),
    }
    Ok(true)
}

fn reconcile_rollback_metadata_publication(path: &Path) -> Result<(), StateError> {
    let backup = rollback_metadata_backup_path(path);
    let previous_sha512 = match fs::symlink_metadata(&backup) {
        Ok(_) => admitted_rollback_file_sha512(&backup)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StateError::Read(error)),
    };
    let current = match fs::symlink_metadata(path) {
        Ok(_) => Some(admitted_rollback_file_sha512(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(StateError::Read(error)),
    };
    match current {
        Some(_) => {
            remove_file_matching_sha512(&backup, &previous_sha512, ROLLBACK_METADATA_MAX_BYTES)
        }
        None => durable_rollback_rename(&backup, path, RollbackDurabilityPoint::LatestRestored)
            .map_err(|failure| failure.error),
    }
}

fn reconcile_rollback_metadata_publication_for_write(path: &Path) -> Result<(), StateError> {
    reconcile_rollback_metadata_publication(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => admitted_rollback_file_sha512(path).map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Read(error)),
    }
}

pub(crate) fn reconcile_rollback_metadata(instance_mods_dir: &Path) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods_dir)?;
    validate_rollback_internal_roots(instance_mods_dir)?;
    reconcile_rollback_metadata_publication(&rollback_file_path(instance_mods_dir))
}

fn publish_rollback_metadata(
    temp_path: &Path,
    path: &Path,
) -> Result<(), RollbackMetadataPublicationFailure> {
    let backup = rollback_metadata_backup_path(path);
    let backup_sha512 = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let admitted_sha512 = admitted_rollback_file_sha512(path)
                .map_err(RollbackMetadataPublicationFailure::PrePublish)?;
            let admitted_identity = admit_file_identity(path)
                .map_err(StateError::Read)
                .map_err(RollbackMetadataPublicationFailure::PrePublish)?;
            durable_rollback_rename(path, &backup, RollbackDurabilityPoint::LatestBackedUp)
                .map_err(|failure| RollbackMetadataPublicationFailure::PrePublish(failure.error))?;
            let backup_matches =
                crate::file_identity::revalidate(&backup, admitted_identity.0, admitted_identity.1)
                    .is_ok()
                    && admitted_rollback_file_sha512(&backup)
                        .is_ok_and(|digest| digest == admitted_sha512);
            if !backup_matches {
                if path_exists(&backup).map_err(RollbackMetadataPublicationFailure::PrePublish)?
                    && !path_exists(path).map_err(RollbackMetadataPublicationFailure::PrePublish)?
                {
                    durable_rollback_rename(&backup, path, RollbackDurabilityPoint::LatestRestored)
                        .map_err(|restore| {
                            RollbackMetadataPublicationFailure::PrePublish(restore.error)
                        })?;
                }
                return Err(RollbackMetadataPublicationFailure::PrePublish(
                    StateError::InvalidRollback(
                        "rollback metadata backup ownership cannot be proven".to_string(),
                    ),
                ));
            }
            Some(admitted_sha512)
        }
        Ok(_) => {
            return Err(RollbackMetadataPublicationFailure::PrePublish(
                StateError::InvalidRollback(
                    "rollback metadata destination is not a regular file".to_string(),
                ),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(RollbackMetadataPublicationFailure::PrePublish(
                StateError::Read(error),
            ));
        }
    };
    if let Err(failure) =
        durable_rollback_rename(temp_path, path, RollbackDurabilityPoint::LatestPublished)
    {
        if failure.renamed {
            return Err(RollbackMetadataPublicationFailure::Indeterminate(
                failure.error,
            ));
        }
        let backup_exists =
            path_exists(&backup).map_err(RollbackMetadataPublicationFailure::PrePublish)?;
        let latest_exists =
            path_exists(path).map_err(RollbackMetadataPublicationFailure::PrePublish)?;
        if backup_exists && !latest_exists {
            durable_rollback_rename(&backup, path, RollbackDurabilityPoint::LatestRestored)
                .map_err(|restore| RollbackMetadataPublicationFailure::PrePublish(restore.error))?;
        }
        return Err(RollbackMetadataPublicationFailure::PrePublish(
            failure.error,
        ));
    }
    if let Some(backup_sha512) = backup_sha512 {
        remove_file_matching_sha512(&backup, &backup_sha512, ROLLBACK_METADATA_MAX_BYTES)
            .map_err(RollbackMetadataPublicationFailure::Indeterminate)?;
    }
    Ok(())
}

fn admitted_rollback_file_sha512(path: &Path) -> Result<String, StateError> {
    let data = read_bounded_regular_metadata_file(path)?;
    let snapshot = serde_json::from_slice::<RollbackSnapshot>(&data)?;
    validate_rollback_snapshot(&snapshot)?;
    Ok(hex::encode(Sha512::digest(data)))
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
    if let Err(error) = file
        .write_all(data.as_bytes())
        .and_then(|()| file.sync_all())
    {
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
    if let Some(snapshot) = load_rollback_snapshot_admitted(instance_mods_dir)? {
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

    let mut count = 0_usize;
    for entry in entries {
        let entry = entry?;
        admit_recovery_entry(&mut count, "retained rollback history entries")?;
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
    for record in snapshots.iter().filter(|record| !record.latest) {
        if keep.contains(&record.snapshot.id) {
            continue;
        }
        let cleanup = stage_pruned_snapshot_artifacts(instance_mods_dir, &record.snapshot)?;
        remove_owned_file(&rollback_history_file_path(
            instance_mods_dir,
            &record.snapshot.id,
        ))?;
        for path in cleanup {
            remove_owned_file(&path)?;
        }
    }
    Ok(())
}

fn stage_pruned_snapshot_artifacts(
    instance_mods_dir: &Path,
    snapshot: &RollbackSnapshot,
) -> Result<Vec<PathBuf>, StateError> {
    let mut cleanup_paths = Vec::new();
    for artifact in &snapshot.artifacts {
        let path = rollback_files_dir_path(instance_mods_dir).join(&artifact.stored_filename);
        if !path_exists(&path)? {
            continue;
        }
        if !path_matches_sha512(&path, &artifact.sha512)? {
            return Err(StateError::InvalidRollback(
                "pruned rollback artifact ownership cannot be proven".to_string(),
            ));
        }
        let cleanup = PathBuf::from(format!("{}.prune-{}.tmp", path.display(), artifact.sha512));
        reserve_backup_exclusive(
            &path,
            &cleanup,
            StatePublicationPhase::Cleanup,
            Some(&artifact.sha512),
        )?;
        cleanup_paths.push(cleanup);
    }
    Ok(cleanup_paths)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        InstalledMod, ManagedArtifactIntegrity, ManagedArtifactProvider, ManagedArtifactRole,
        ManagedArtifactSource, VersionFamily,
    };

    fn restored_composition(outcome: ManagedRollbackOutcome) -> CompositionState {
        match outcome {
            ManagedRollbackOutcome::ManagedComposition(state) => state,
            ManagedRollbackOutcome::ManagedStateAbsent => {
                panic!("expected rollback to restore a managed composition")
            }
        }
    }

    fn with_rollback_durability_failure<T>(
        point: RollbackDurabilityPoint,
        operation: impl FnOnce() -> T,
    ) -> T {
        ROLLBACK_DURABILITY_FAILURE.with(|failure| {
            assert_eq!(failure.replace(Some(point)), None);
        });
        let result = operation();
        ROLLBACK_DURABILITY_FAILURE.with(|failure| {
            assert_eq!(failure.replace(None), None, "fault point was not reached");
        });
        result
    }

    #[test]
    fn admitted_state_read_does_not_reconcile_staged_publication() {
        let root = test_root("admitted-state-read");
        let staged_state = test_state("staged-core", Vec::new());
        let staged = PersistedCompositionState {
            schema_version: STATE_SCHEMA_VERSION,
            state: staged_state.clone(),
        };
        fs::write(
            state_staged_path(&root),
            serde_json::to_vec_pretty(&staged).expect("serialize staged state"),
        )
        .expect("write staged state");

        assert!(
            load_state_admitted(&root)
                .expect("read admitted state")
                .is_none()
        );
        assert!(state_staged_path(&root).exists());
        assert!(!lock_file_path(&root).exists());

        reconcile_managed_storage(&root).expect("reconcile managed storage");

        assert_eq!(
            load_state_admitted(&root)
                .expect("read reconciled state")
                .expect("published state"),
            staged_state
        );
        assert!(!state_staged_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn admitted_state_read_does_not_settle_removal_obligation() {
        let root = test_root("admitted-removal-read");
        let installed = test_mod("sodium", "managed-a.jar");
        fs::write(root.join(&installed.filename), b"managed-a").expect("write managed artifact");
        save_state(&root, &test_state("core", vec![installed.clone()]))
            .expect("save managed state");
        let backup = stage_managed_artifact_removal(&root, &installed)
            .expect("stage managed artifact removal");

        assert!(
            load_state_admitted(&root)
                .expect("read admitted state")
                .is_some()
        );
        assert!(backup.exists());
        assert!(!root.join(&installed.filename).exists());

        reconcile_managed_storage(&root).expect("reconcile managed storage");

        assert!(!backup.exists());
        assert_eq!(
            fs::read(root.join(&installed.filename)).expect("read restored managed artifact"),
            b"managed-a"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inspection_preflight_detects_removal_without_settling_it() {
        let root = test_root("preflight-removal-obligation");
        let installed = test_mod("sodium", "managed-a.jar");
        fs::write(root.join(&installed.filename), b"managed-a").expect("write managed artifact");
        save_state(&root, &test_state("core", vec![installed.clone()]))
            .expect("save managed state");
        let backup = stage_managed_artifact_removal(&root, &installed)
            .expect("stage managed artifact removal");

        let preflight = preflight_managed_inspection_reconciliation(&root)
            .expect("preflight removal obligation");

        assert!(preflight.admitted_state_reconciliation_required());
        assert!(backup.exists());
        assert!(!root.join(&installed.filename).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_recovery_settles_exact_removal_obligation_and_proves_state() {
        let root = test_root("recover-removal-obligation");
        let installed = test_mod("sodium", "managed-a.jar");
        fs::write(root.join(&installed.filename), b"managed-a").expect("write managed artifact");
        save_state(&root, &test_state("core", vec![installed.clone()]))
            .expect("save managed state");
        let backup = stage_managed_artifact_removal(&root, &installed)
            .expect("stage managed artifact removal");

        let state = recover_managed_storage(&root)
            .expect("recover managed storage")
            .expect("managed state");
        prove_managed_storage_recovered(&root, Some(&state)).expect("prove recovered storage");

        assert_eq!(
            fs::read(root.join(&installed.filename)).expect("read restored artifact"),
            b"managed-a"
        );
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_recovery_preserves_unknown_restore_temp() {
        let root = test_root("recover-unknown-restore-temp");
        let rollback_tmp = rollback_tmp_dir_path(&root);
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let unknown = rollback_tmp.join("unknown-restore.tmp");
        fs::write(&unknown, b"unknown").expect("write unknown restore temp");

        let error = recover_managed_storage(&root).expect_err("unknown temp must block recovery");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert_eq!(fs::read(unknown).expect("read preserved temp"), b"unknown");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovered_storage_proof_requires_every_retained_rollback_artifact() {
        let root = test_root("recover-missing-retained-artifact");
        let installed = test_mod("sodium", "managed-a.jar");
        fs::write(root.join(&installed.filename), b"managed-a").expect("write managed artifact");
        let state = test_state("core", vec![installed]);
        save_state(&root, &state).expect("save managed state");
        let snapshot = save_rollback_snapshot(&root, &state).expect("save rollback snapshot");
        let artifact_path =
            rollback_files_dir_path(&root).join(&snapshot.artifacts[0].stored_filename);
        fs::remove_file(&artifact_path).expect("remove retained artifact");

        let error = prove_managed_storage_recovered(&root, Some(&state))
            .expect_err("missing retained artifact must fail proof");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert!(rollback_file_path(&root).exists());
        assert!(!artifact_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn addition_recovery_does_not_claim_legacy_filename_derived_temp_alias() {
        let root = test_root("recover-uncommitted-addition");
        let filename = "new.jar";
        let digest = hex::encode(Sha512::digest(b"new-managed"));
        let temp = root.join("new.jar.sodium.tmp");
        let final_path = root.join(filename);
        fs::write(&temp, b"new-managed").expect("write managed temp");
        let obligation = stage_managed_artifact_addition(&root, filename, &digest, &temp)
            .expect("stage addition obligation");
        fs::hard_link(&obligation, &final_path).expect("publish managed final");

        assert!(
            recover_managed_storage(&root)
                .expect("recover uncommitted addition")
                .is_none()
        );

        assert!(!final_path.exists());
        assert_eq!(
            fs::read(&temp).expect("legacy temp remains user-owned"),
            b"new-managed"
        );
        assert!(!obligation.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn obsolete_replacement_namespace_is_rejected_and_preserved() {
        let root = test_root("obsolete-replacement-namespace");
        let replacements = root
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join("replacements");
        fs::create_dir_all(&replacements).expect("create obsolete namespace");
        let unknown = replacements.join("unknown-owned-entry");
        fs::write(&unknown, b"unknown").expect("write unknown entry");

        let error = prove_managed_storage_recovered(&root, None)
            .expect_err("obsolete namespace must fail closed");

        assert!(matches!(error, StateError::InvalidState(_)));
        assert_eq!(
            fs::read(unknown).expect("unknown entry preserved"),
            b"unknown"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn addition_recovery_reconstructs_committed_missing_final() {
        let root = test_root("recover-committed-addition");
        let filename = "new.jar";
        let digest = hex::encode(Sha512::digest(b"new-managed"));
        let temp = root.join("download.tmp");
        fs::write(&temp, b"new-managed").expect("write managed temp");
        let obligation = stage_managed_artifact_addition(&root, filename, &digest, &temp)
            .expect("stage addition obligation");
        fs::remove_file(&temp).expect("consume managed temp");
        let mut installed = test_mod("sodium", filename);
        installed.integrity.sha512 = digest;
        save_state(&root, &test_state("core", vec![installed])).expect("commit managed state");

        let recovered = recover_managed_storage(&root)
            .expect("recover committed addition")
            .expect("committed state");

        assert_eq!(
            fs::read(root.join(filename)).expect("read final"),
            b"new-managed"
        );
        assert_eq!(recovered.installed_mods.len(), 1);
        assert!(!obligation.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_addition_obligation_recovers_before_and_after_state_commit() {
        for committed in [false, true] {
            let root = test_root(if committed {
                "recover-rollback-addition-committed"
            } else {
                "recover-rollback-addition-uncommitted"
            });
            let installed = test_mod("sodium", "managed-a.jar");
            fs::write(root.join(&installed.filename), b"managed-a").expect("write rollback source");
            let target_state = test_state("rollback-target", vec![installed.clone()]);
            let snapshot =
                save_rollback_snapshot(&root, &target_state).expect("save rollback snapshot");
            fs::remove_file(root.join(&installed.filename)).expect("remove rollback source target");
            let current_state = test_state("current", Vec::new());
            save_state(&root, &current_state).expect("save current state");
            let targets = prepare_rollback_restore_targets(
                &root,
                &snapshot,
                &managed_artifacts(Some(&current_state)),
            )
            .expect("prepare rollback targets");
            stage_rollback_restore_targets(&targets).expect("stage rollback targets");
            publish_rollback_restore_target(&targets[0]).expect("publish rollback addition");
            if committed {
                save_state(&root, &target_state).expect("commit rollback state");
            }

            let recovered = recover_managed_storage(&root).expect("recover rollback addition");

            if committed {
                assert_eq!(recovered, Some(target_state));
                assert_eq!(
                    fs::read(root.join(&installed.filename)).expect("read committed target"),
                    b"managed-a"
                );
            } else {
                assert_eq!(recovered, Some(current_state));
                assert!(!root.join(&installed.filename).exists());
            }
            assert!(
                !root
                    .join(STATE_DIR_NAME)
                    .join(MUTATION_DIR_NAME)
                    .join(ADDITION_DIR_NAME)
                    .exists()
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn addition_recovery_preserves_same_bytes_different_identity_replacement() {
        let root = test_root("recover-addition-same-bytes-replacement");
        let filename = "new.jar";
        let digest = hex::encode(Sha512::digest(b"same-bytes"));
        let source = root.join("source.download");
        fs::write(&source, b"same-bytes").expect("write source");
        let obligation = stage_managed_artifact_addition(&root, filename, &digest, &source)
            .expect("stage addition obligation");
        fs::remove_file(source).expect("consume source");
        fs::write(root.join(filename), b"same-bytes").expect("write replacement inode");

        let recovered = recover_managed_storage(&root).expect("discard owned obligation");

        assert!(recovered.is_none());
        assert_eq!(
            fs::read(root.join(filename)).expect("read replacement"),
            b"same-bytes"
        );
        assert!(!obligation.exists());
        assert!(
            !root
                .join(STATE_DIR_NAME)
                .join(MUTATION_DIR_NAME)
                .join(ADDITION_DIR_NAME)
                .exists()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn addition_recovery_rejects_case_colliding_obligations_before_mutation() {
        let root = test_root("recover-addition-case-collision");
        let first_digest = hex::encode(Sha512::digest(b"first"));
        let second_digest = hex::encode(Sha512::digest(b"second"));
        let first_source = root.join("first.download");
        let second_source = root.join("second.download");
        fs::write(&first_source, b"first").expect("write first source");
        fs::write(&second_source, b"second").expect("write second source");
        let first =
            stage_managed_artifact_addition(&root, "Case.jar", &first_digest, &first_source)
                .expect("stage first addition");
        let second =
            stage_managed_artifact_addition(&root, "case.jar", &second_digest, &second_source)
                .expect("stage second addition");

        let error = reconcile_managed_addition_obligations(&root, None)
            .expect_err("case collision must block recovery");

        assert!(matches!(error, StateError::InvalidState(_)));
        assert!(first.exists());
        assert!(second.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_recovery_counts_empty_digest_directories() {
        let root = test_root("recover-removal-entry-limit");
        let removals = root
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join(REMOVAL_DIR_NAME);
        fs::create_dir_all(&removals).expect("create removal root");
        for index in 0..=RECOVERY_ENTRY_LIMIT {
            fs::create_dir(removals.join(format!("{index:0128x}")))
                .expect("create removal digest directory");
        }

        let error = recover_managed_storage(&root).expect_err("entry limit must block recovery");

        assert!(matches!(error, StateError::InvalidState(_)));
        assert_eq!(
            fs::read_dir(removals).expect("read removal root").count(),
            RECOVERY_ENTRY_LIMIT + 1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_recovery_settles_exact_empty_digest_directories() {
        let root = test_root("recover-empty-removal-digests");
        let removals = root
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join(REMOVAL_DIR_NAME);
        ensure_mutation_directory_tree(&root, &removals).expect("create removal root");
        for index in 0..8 {
            create_reserved_directory(&removals.join(format!("{index:0128x}")))
                .expect("create empty digest directory");
        }

        recover_managed_storage(&root).expect("recover empty removal digests");

        assert_eq!(
            fs::read_dir(&removals).expect("read removal root").count(),
            0
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repeated_removal_settlement_does_not_accumulate_digest_directories() {
        let root = test_root("bounded-removal-settlement");
        let installed = test_mod("sodium", "managed-a.jar");
        let removals = root
            .join(STATE_DIR_NAME)
            .join(MUTATION_DIR_NAME)
            .join(REMOVAL_DIR_NAME);

        for _ in 0..8 {
            fs::write(root.join(&installed.filename), b"managed-a")
                .expect("write managed artifact");
            let backup = stage_managed_artifact_removal(&root, &installed)
                .expect("stage managed artifact removal");
            settle_managed_artifact_removal(&root, &installed)
                .expect("settle managed artifact removal");

            assert!(!backup.exists());
            assert_eq!(
                fs::read_dir(&removals).expect("read removal root").count(),
                0
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovered_storage_rejects_divergent_latest_history_payload() {
        let root = test_root("recover-divergent-latest-history");
        let snapshot = save_rollback_snapshot(&root, &test_state("core", Vec::new()))
            .expect("save rollback snapshot");
        let history = rollback_history_file_path(&root, &snapshot.id);
        fs::remove_file(&history).expect("remove linked history");
        let mut divergent = snapshot.clone();
        divergent.created_at = "2026-05-31T00:00:00Z".to_string();
        fs::write(
            &history,
            serde_json::to_vec_pretty(&divergent).expect("serialize divergent history"),
        )
        .expect("write divergent history");

        let error = prove_managed_storage_recovered(&root, None)
            .expect_err("divergent latest and history must fail proof");

        assert!(matches!(error, StateError::InvalidRollback(_)));
        assert!(rollback_file_path(&root).exists());
        assert!(history.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn admitted_rollback_read_does_not_reconcile_metadata_publication() {
        let root = test_root("admitted-rollback-read");
        let snapshot = save_rollback_snapshot(&root, &test_state("core", Vec::new()))
            .expect("save rollback snapshot");
        let latest = rollback_file_path(&root);
        let backup = rollback_metadata_backup_path(&latest);
        fs::rename(&latest, &backup).expect("stage rollback metadata obligation");

        assert!(
            load_rollback_snapshot_admitted(&root)
                .expect("read admitted rollback")
                .is_none()
        );
        assert!(backup.exists());
        assert!(!latest.exists());

        reconcile_managed_storage(&root).expect("reconcile managed storage");

        assert_eq!(
            load_rollback_snapshot_admitted(&root)
                .expect("read reconciled rollback")
                .expect("restored rollback")
                .id,
            snapshot.id
        );
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inspection_preflight_detects_rollback_publication_without_settling_it() {
        let root = test_root("preflight-rollback-publication");
        save_rollback_snapshot(&root, &test_state("core", Vec::new()))
            .expect("save rollback snapshot");
        let latest = rollback_file_path(&root);
        let backup = rollback_metadata_backup_path(&latest);
        fs::rename(&latest, &backup).expect("stage rollback metadata obligation");

        let preflight = preflight_managed_inspection_reconciliation(&root)
            .expect("preflight rollback obligation");

        assert!(preflight.admitted_state_reconciliation_required());
        assert!(backup.exists());
        assert!(!latest.exists());
        let _ = fs::remove_dir_all(root);
    }

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

        let summaries = list_rollback_snapshots_admitted(&root).expect("list rollback snapshots");
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

        let summaries = list_rollback_snapshots_admitted(&root).expect("list rollback snapshots");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, snapshot.id);
        assert_eq!(summaries[0].installed_count, 0);
        assert_eq!(summaries[0].artifact_count, 0);
        assert!(summaries[0].rollback_available);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn absent_rollback_removes_partially_promoted_addition_and_preserves_user_files() {
        let root = test_root("absent-rollback-partial-addition");
        fs::write(root.join("user.jar"), b"user-v1").expect("write user file");
        let snapshot = save_absent_rollback_snapshot(&root).expect("save absent snapshot");
        let source = root.join("managed.jar.project.tmp");
        fs::write(&source, b"managed-partial").expect("write managed source");
        let digest = hex::encode(Sha512::digest(b"managed-partial"));
        let obligation = stage_managed_artifact_addition(&root, "managed.jar", &digest, &source)
            .expect("stage managed addition");
        fs::hard_link(&obligation, root.join("managed.jar"))
            .expect("publish partial managed addition");
        fs::remove_file(source).expect("settle managed download temp");

        let recovered = recover_managed_storage(&root)
            .expect("recover partial promotion after indeterminate install");

        assert!(recovered.is_none());
        assert!(!root.join("managed.jar").exists());
        assert!(!lock_file_path(&root).exists());
        assert_eq!(
            fs::read(root.join("user.jar")).expect("read user file"),
            b"user-v1"
        );
        assert!(
            !root
                .join(STATE_DIR_NAME)
                .join(MUTATION_DIR_NAME)
                .join(ADDITION_DIR_NAME)
                .exists()
        );
        let outcome = restore_rollback_snapshot(&root, &snapshot)
            .expect("restore retained absence after recovery");
        assert_eq!(outcome, ManagedRollbackOutcome::ManagedStateAbsent);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_rejects_missing_artifact_metadata() {
        let root = test_root("missing-rollback-artifact-metadata");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
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
                "target": {
                    "kind": "managed_composition",
                    "state": test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
                },
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
        assert_eq!(
            artifact.project_id,
            test_mod("sodium", "managed-a.jar").project_id
        );
        assert_eq!(artifact.version_id, "NFkjnzWE");
        assert_eq!(artifact.ownership_class, OwnershipClass::CompositionManaged);
        assert_eq!(
            artifact.sha512,
            test_mod("sodium", "managed-a.jar").integrity.sha512
        );
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
    fn rollback_snapshot_is_independent_from_live_in_place_corruption() {
        let root = test_root("snapshot-independent-copy");
        let live_path = root.join("managed-a.jar");
        fs::write(&live_path, b"managed-a").expect("write managed artifact");
        let live = crate::file_identity::admit(&live_path).expect("admit live artifact");
        let live_identity = live.identity();
        let live_len = live.metadata().len();
        drop(live);

        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save rollback snapshot");
        let stored_path =
            rollback_files_dir_path(&root).join(&snapshot.artifacts[0].stored_filename);
        let stored = crate::file_identity::admit(&stored_path).expect("admit stored artifact");
        assert_ne!(stored.identity(), live_identity);
        drop(stored);

        let mut live = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&live_path)
            .expect("open live artifact in place");
        live.write_all(b"corrupt-a")
            .and_then(|()| live.sync_all())
            .expect("corrupt live artifact in place");
        drop(live);
        crate::file_identity::revalidate(&live_path, live_identity, live_len)
            .expect("live corruption retained physical identity");

        assert_eq!(
            fs::read(&stored_path).expect("read independent stored artifact"),
            b"managed-a"
        );
        assert!(
            path_matches_sha512(&stored_path, &snapshot.artifacts[0].sha512)
                .expect("verify retained rollback artifact")
        );

        fs::remove_file(&live_path).expect("remove corrupt live artifact");
        let restored = restored_composition(
            restore_rollback_snapshot(&root, &snapshot).expect("restore independent snapshot"),
        );
        assert_eq!(restored.composition_id, "core-a");
        assert_eq!(
            fs::read(live_path).expect("read restored artifact"),
            b"managed-a"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_cleans_artifacts_when_files_directory_sync_fails() {
        let root = test_root("snapshot-files-sync-failure");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);

        let error = with_rollback_durability_failure(
            RollbackDurabilityPoint::ArtifactCopiesPublished,
            || save_rollback_snapshot(&root, &state),
        )
        .expect_err("files directory sync failure must reject snapshot");

        assert!(matches!(error, StateError::Read(_)));
        assert_eq!(
            fs::read_dir(rollback_files_dir_path(&root))
                .expect("read rollback files")
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(rollback_history_dir_path(&root))
                .expect("read rollback history")
                .count(),
            0
        );
        assert!(!rollback_file_path(&root).exists());
        save_rollback_snapshot(&root, &state).expect("retry snapshot after settled cleanup");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_cleans_candidate_when_history_directory_sync_fails() {
        let root = test_root("snapshot-history-sync-failure");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);

        let error =
            with_rollback_durability_failure(RollbackDurabilityPoint::HistoryStaged, || {
                save_rollback_snapshot(&root, &state)
            })
            .expect_err("history directory sync failure must reject snapshot");

        assert!(matches!(error, StateError::Read(_)));
        assert_eq!(
            fs::read_dir(rollback_files_dir_path(&root))
                .expect("read rollback files")
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(rollback_history_dir_path(&root))
                .expect("read rollback history")
                .count(),
            0
        );
        assert!(!rollback_file_path(&root).exists());
        save_rollback_snapshot(&root, &state).expect("retry snapshot after settled cleanup");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_preserves_canonical_history_after_latest_sync_failure() {
        let root = test_root("snapshot-latest-sync-failure");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);

        let error =
            with_rollback_durability_failure(RollbackDurabilityPoint::LatestPublished, || {
                save_rollback_snapshot(&root, &state)
            })
            .expect_err("latest sync failure must report an indeterminate publication");

        assert!(matches!(error, StateError::Read(_)));
        let latest = load_rollback_snapshot(&root)
            .expect("load latest after reported sync failure")
            .expect("latest remains readable");
        let history = rollback_history_file_path(&root, &latest.id);
        assert_eq!(
            read_bounded_regular_metadata_file(&history).expect("read canonical history"),
            read_bounded_regular_metadata_file(&rollback_file_path(&root)).expect("read latest")
        );
        assert!(
            rollback_files_dir_path(&root)
                .join(&latest.artifacts[0].stored_filename)
                .exists(),
            "indeterminate latest publication must retain backing artifacts"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_snapshot_preserves_history_when_latest_backup_sync_fails() {
        let root = test_root("snapshot-backup-sync-failure");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let retained = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save retained snapshot");
        let successor_state = test_state("core-b", vec![test_mod("sodium", "managed-a.jar")]);

        let error =
            with_rollback_durability_failure(RollbackDurabilityPoint::LatestBackedUp, || {
                save_rollback_snapshot(&root, &successor_state)
            })
            .expect_err("latest backup sync failure must reject publication");

        assert!(matches!(error, StateError::Read(_)));
        assert!(!rollback_file_path(&root).exists());
        assert!(rollback_metadata_backup_path(&rollback_file_path(&root)).exists());
        reconcile_rollback_metadata(&root).expect("restore retained latest from backup");
        assert_eq!(
            load_rollback_snapshot_admitted(&root)
                .expect("load reconciled latest")
                .expect("reconciled latest exists")
                .id,
            retained.id
        );
        let records = load_retained_rollback_snapshots(&root).expect("load retained history");
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| {
            record.snapshot.artifacts.iter().all(|artifact| {
                rollback_files_dir_path(&root)
                    .join(&artifact.stored_filename)
                    .exists()
            })
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_backup_reconciliation_survives_restore_sync_failure() {
        let root = test_root("snapshot-backup-reconcile-sync-failure");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let retained = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save retained snapshot");
        let latest = rollback_file_path(&root);
        let backup = rollback_metadata_backup_path(&latest);
        fs::rename(&latest, &backup).expect("stage backup reconciliation obligation");

        let error =
            with_rollback_durability_failure(RollbackDurabilityPoint::LatestRestored, || {
                reconcile_rollback_metadata(&root)
            })
            .expect_err("restored latest sync failure must remain recoverable");

        assert!(matches!(error, StateError::Read(_)));
        assert!(latest.exists());
        assert!(!backup.exists());
        reconcile_rollback_metadata(&root).expect("reconcile already-restored latest");
        assert_eq!(
            load_rollback_snapshot_admitted(&root)
                .expect("load latest")
                .expect("latest exists")
                .id,
            retained.id
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_history_publication_preserves_preexisting_canonical_collision() {
        let root = test_root("snapshot-history-collision");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);
        let (planned, snapshot) = test_snapshot_candidate(&root, &state, "rb-history-collision");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let collision = rollback_history_file_path(&root, &snapshot.id);
        fs::write(&collision, b"user-collision").expect("write canonical collision");

        commit_rollback_snapshot(&root, &planned, &snapshot)
            .expect_err("canonical history collision must reject publication");

        assert_eq!(
            fs::read(&collision).expect("read preserved canonical collision"),
            b"user-collision"
        );
        assert_eq!(
            fs::read_dir(rollback_files_dir_path(&root))
                .expect("read rollback files")
                .count(),
            0
        );
        assert!(!rollback_file_path(&root).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_latest_backup_reservation_preserves_preexisting_collision() {
        let root = test_root("snapshot-backup-collision");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save retained snapshot");
        let latest = rollback_file_path(&root);
        let latest_before = fs::read(&latest).expect("read retained latest");
        let backup = rollback_metadata_backup_path(&latest);
        fs::write(&backup, b"user-backup-collision").expect("write backup collision");
        let successor = RollbackSnapshot {
            id: "rb-backup-collision-successor".to_string(),
            schema_version: ROLLBACK_SCHEMA_VERSION,
            created_at: "2026-05-30T00:00:01Z".to_string(),
            target: RollbackSnapshotState::ManagedStateAbsent,
            artifacts: Vec::new(),
        };
        let temp = latest.with_extension("json.tmp");
        let mut temp_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .expect("create latest candidate");
        temp_file
            .write_all(&serde_json::to_vec_pretty(&successor).expect("serialize successor"))
            .and_then(|()| temp_file.sync_all())
            .expect("sync latest candidate");
        drop(temp_file);

        publish_rollback_metadata(&temp, &latest)
            .expect_err("backup collision must reject publication");

        assert_eq!(
            fs::read(&latest).expect("read preserved latest"),
            latest_before
        );
        assert_eq!(
            fs::read(&backup).expect("read preserved backup collision"),
            b"user-backup-collision"
        );
        assert!(temp.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_artifact_publication_preserves_preexisting_final_collision() {
        let root = test_root("snapshot-artifact-collision");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);
        let (planned, snapshot) = test_snapshot_candidate(&root, &state, "rb-artifact-collision");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let final_path = planned.artifacts[0].stored_path.clone();
        let staged_path = planned.artifacts[0].staged_path.clone();
        fs::write(&final_path, b"user-collision").expect("write artifact collision");

        commit_rollback_snapshot(&root, &planned, &snapshot)
            .expect_err("artifact collision must reject publication");

        assert_eq!(
            fs::read(final_path).expect("read preserved artifact collision"),
            b"user-collision"
        );
        assert!(!staged_path.exists());
        assert!(!rollback_history_file_path(&root, &snapshot.id).exists());
        assert!(
            !rollback_history_file_path(&root, &snapshot.id)
                .with_extension("json.new.tmp")
                .exists(),
            "namespace preflight must reject before staging an ownership manifest"
        );
        assert!(!rollback_file_path(&root).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_artifact_preflight_preserves_exact_content_collision() {
        let root = test_root("snapshot-artifact-exact-collision");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);
        let (planned, snapshot) =
            test_snapshot_candidate(&root, &state, "rb-artifact-exact-collision");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let final_path = planned.artifacts[0].stored_path.clone();
        fs::write(&final_path, b"managed-a").expect("write exact-content collision");
        let collision = crate::file_identity::admit(&final_path).expect("admit collision");
        let collision_identity = collision.identity();
        let collision_len = collision.metadata().len();
        drop(collision);
        ROLLBACK_DURABILITY_FAILURE.with(|failure| {
            assert_eq!(
                failure.replace(Some(RollbackDurabilityPoint::HistoryStaged)),
                None
            );
        });

        commit_rollback_snapshot(&root, &planned, &snapshot)
            .expect_err("exact-content collision must reject before candidate publication");
        ROLLBACK_DURABILITY_FAILURE.with(|failure| {
            assert_eq!(
                failure.replace(None),
                Some(RollbackDurabilityPoint::HistoryStaged),
                "namespace collision must reject before the history sync fault point"
            );
        });

        crate::file_identity::revalidate(&final_path, collision_identity, collision_len)
            .expect("exact-content collision identity must remain untouched");
        assert_eq!(
            fs::read(&final_path).expect("read exact-content collision"),
            b"managed-a"
        );
        assert!(!planned.artifacts[0].staged_path.exists());
        assert!(!rollback_history_file_path(&root, &snapshot.id).exists());
        assert!(
            !rollback_history_file_path(&root, &snapshot.id)
                .with_extension("json.new.tmp")
                .exists(),
            "collision rejection must not leave a candidate ownership manifest"
        );
        assert!(!rollback_file_path(&root).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rollback_artifact_preflight_preserves_portable_name_alias() {
        let root = test_root("snapshot-artifact-portable-alias");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);
        let (planned, snapshot) =
            test_snapshot_candidate(&root, &state, "rb-artifact-portable-alias");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let alias_name = planned.artifacts[0]
            .stored_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("candidate filename")
            .to_ascii_uppercase();
        let alias = rollback_files_dir_path(&root).join(alias_name);
        fs::write(&alias, b"portable-alias").expect("write portable alias");

        commit_rollback_snapshot(&root, &planned, &snapshot)
            .expect_err("portable name alias must reject before candidate publication");

        assert_eq!(
            fs::read(alias).expect("read preserved portable alias"),
            b"portable-alias"
        );
        assert!(
            !rollback_history_file_path(&root, &snapshot.id)
                .with_extension("json.new.tmp")
                .exists()
        );
        assert!(!rollback_file_path(&root).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_recovery_cleans_manifest_bound_partial_artifact_publication() {
        let root = test_root("snapshot-partial-artifact-recovery");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        fs::write(root.join("managed-b.jar"), b"managed-b").expect("write managed b");
        let state = test_state(
            "core-a",
            vec![
                test_mod("sodium", "managed-a.jar"),
                test_mod("lithium", "managed-b.jar"),
            ],
        );
        let (planned, snapshot) =
            test_snapshot_candidate(&root, &state, "rb-partial-artifact-recovery");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let history = rollback_history_file_path(&root, &snapshot.id);
        let history_temp = stage_new_rollback_snapshot(&history, &snapshot)
            .expect("stage candidate ownership manifest");
        sync_rollback_directory(
            &rollback_history_dir_path(&root),
            RollbackDurabilityPoint::HistoryStaged,
        )
        .expect("sync candidate ownership manifest");
        stage_rollback_artifact_copy(&planned.artifacts[0]).expect("stage first artifact");
        durable_rollback_rename(
            &planned.artifacts[0].staged_path,
            &planned.artifacts[0].stored_path,
            RollbackDurabilityPoint::ArtifactPublished,
        )
        .expect("publish first artifact");
        let mut partial = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&planned.artifacts[1].staged_path)
            .expect("create partial second artifact");
        partial
            .write_all(b"partial")
            .and_then(|()| partial.sync_all())
            .expect("sync partial second artifact");
        drop(partial);

        recover_managed_storage(&root).expect("recover partial candidate");

        assert!(!history_temp.exists());
        assert!(!planned.artifacts[0].stored_path.exists());
        assert!(!planned.artifacts[1].staged_path.exists());
        assert!(!history.exists());
        assert!(!rollback_file_path(&root).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_recovery_retains_manifest_until_artifact_cleanup_is_synced() {
        let root = test_root("snapshot-cleanup-ordering");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let state = test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]);
        let (planned, snapshot) = test_snapshot_candidate(&root, &state, "rb-cleanup-ordering");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let history = rollback_history_file_path(&root, &snapshot.id);
        let history_temp = stage_new_rollback_snapshot(&history, &snapshot)
            .expect("stage candidate ownership manifest");
        stage_rollback_artifact_copy(&planned.artifacts[0]).expect("stage artifact");
        durable_rollback_rename(
            &planned.artifacts[0].staged_path,
            &planned.artifacts[0].stored_path,
            RollbackDurabilityPoint::ArtifactPublished,
        )
        .expect("publish artifact");

        with_rollback_durability_failure(RollbackDurabilityPoint::ArtifactCopiesCleaned, || {
            recover_managed_storage(&root)
        })
        .expect_err("unsynced artifact cleanup must retain its ownership manifest");

        assert!(history_temp.exists());
        assert!(!history.exists());
        recover_managed_storage(&root).expect("retry manifest-bound cleanup");
        assert!(!history_temp.exists());
        assert!(!planned.artifacts[0].stored_path.exists());
        assert!(!rollback_file_path(&root).exists());

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
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
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
    fn rollback_snapshot_preserves_unproven_orphan_artifact() {
        let root = test_root("snapshot-unproven-orphan");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        let orphan = rollback_files_dir_path(&root).join("rb-orphan-0.bin");
        fs::write(&orphan, b"partial-candidate").expect("write orphan candidate");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");

        save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("unproven orphan should not block bounded successor storage");

        assert_eq!(
            fs::read(&orphan).expect("read preserved orphan"),
            b"partial-candidate"
        );
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
    fn rollback_snapshot_does_not_start_candidate_when_latest_temp_is_invalid() {
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
        fs::remove_dir(rollback_file_path(&root).with_extension("json.tmp"))
            .expect("remove latest temp blocker");

        let latest = load_rollback_snapshot(&root)
            .expect("load retained latest")
            .expect("retained latest exists");
        assert_eq!(latest.id, retained.id);
        assert_eq!(
            fs::read_dir(rollback_files_dir_path(&root))
                .expect("read rollback files")
                .count(),
            1,
            "preflight failure must not create a candidate artifact"
        );
        assert_eq!(
            fs::read_dir(rollback_history_dir_path(&root))
                .expect("read rollback history")
                .count(),
            1,
            "preflight failure must not create candidate history"
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
        let restored = restored_composition(
            restore_rollback_snapshot(&root, &snapshot).expect("restore older"),
        );

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
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
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
            b"managed-a"
        );
        assert!(load_state(&root).expect("load state").is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_refuses_to_overwrite_untracked_existing_target() {
        let root = test_root("restore-untracked-existing-target");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("replace target");

        let error = restore_rollback_snapshot_classified(&root, &snapshot)
            .expect_err("rollback must not overwrite untracked target");

        assert!(matches!(
            error,
            RollbackRestoreError::Definite(StateError::InvalidRollback(_))
        ));
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
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("replace target");
        let mut corrupt = test_state("core-current", vec![test_mod("sodium", "managed-a.jar")]);
        corrupt.installed_mods[0].ownership_class = OwnershipClass::UserManaged;
        write_unvalidated_state(&root, corrupt);
        let artifact_path =
            rollback_files_dir_path(&root).join(&snapshot.artifacts[0].stored_filename);
        let artifact_before = fs::read(&artifact_path).expect("read snapshot artifact before");
        let state_before = fs::read(lock_file_path(&root)).expect("read corrupt state before");

        let error = restore_rollback_snapshot(&root, &snapshot)
            .expect_err("corrupt current ownership must block rollback");

        assert!(matches!(error, StateError::InvalidOwnership { .. }));
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read target"),
            b"user-replacement"
        );
        assert_eq!(
            fs::read(artifact_path).expect("read snapshot artifact after"),
            artifact_before
        );
        assert_eq!(
            fs::read(lock_file_path(&root)).expect("read corrupt state after"),
            state_before
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
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::remove_file(root.join("managed-a.jar")).expect("remove target");
        let user_temp_path = root.join("managed-a.rollback.tmp");
        fs::write(&user_temp_path, b"user-temp").expect("write user temp");

        let restored = restored_composition(
            restore_rollback_snapshot(&root, &snapshot).expect("restore should use managed temp"),
        );

        assert_eq!(restored.composition_id, "core-a");
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read restored"),
            b"managed-a"
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

        let restored = restored_composition(
            restore_rollback_snapshot_classified_async(&root, &snapshot)
                .await
                .expect("restore async"),
        );

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
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let snapshot = save_rollback_snapshot(
            &root,
            &test_state("core-a", vec![test_mod("sodium", "managed-a.jar")]),
        )
        .expect("save snapshot");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("replace target");

        let error = restore_rollback_snapshot_classified_async(&root, &snapshot)
            .await
            .expect_err("async rollback must not overwrite untracked target");

        assert!(matches!(
            error,
            RollbackRestoreError::Definite(StateError::InvalidRollback(_))
        ));
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
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "" }
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            })))
            .expect("serialize state"),
        )
        .expect("write missing ownership state");
        assert!(matches!(
            load_state(&root).expect_err("missing ownership should be invalid"),
            StateError::Parse(_)
        ));

        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "plugin_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "" }
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            })))
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
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "composition_managed"
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            })))
            .expect("serialize state"),
        )
        .expect("write missing source state");
        assert!(matches!(
            load_state(&root).expect_err("missing source and integrity should be invalid"),
            StateError::Parse(_)
        ));

        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "composition_managed",
                    "source": { "provider": "unknown" },
                    "integrity": { "sha512": "" }
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            })))
            .expect("serialize state"),
        )
        .expect("write unknown source state");
        assert!(matches!(
            load_state(&root).expect_err("unknown source should be invalid"),
            StateError::Parse(_)
        ));

        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "sodium.jar",
                    "ownership_class": "composition_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "path": "/tmp/sodium.jar" }
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            })))
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
    fn load_state_rejects_missing_graph_metadata() {
        let root = test_root("missing-graph-metadata");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [],
                "installed_at": "2026-05-30T00:00:00Z"
            })))
            .expect("serialize state"),
        )
        .expect("write state without graph metadata");

        assert!(matches!(
            load_state(&root).expect_err("missing graph metadata should be invalid"),
            StateError::Parse(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_unknown_top_level_fields() {
        let root = test_root("unknown-top-level-state");
        fs::write(
            lock_file_path(&root),
            serde_json::to_vec(&state_fixture(serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [],
                "installed_at": "2026-05-30T00:00:00Z",
                "unexpected_state": true
            })))
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
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        fs::write(
            rollback_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "id": "rb-missing-artifacts",
                "schema_version": ROLLBACK_SCHEMA_VERSION,
                "created_at": "2026-05-30T00:00:00Z",
                "target": {
                    "kind": "managed_composition",
                    "state": test_state("core", Vec::new()),
                }
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
    fn rollback_snapshot_rejects_previous_schema_without_compatibility_parsing() {
        let root = test_root("previous-rollback-schema");
        ensure_rollback_internal_roots(&root).expect("create rollback roots");
        fs::write(
            rollback_file_path(&root),
            serde_json::to_vec(&serde_json::json!({
                "id": "rb-previous-schema",
                "schema_version": ROLLBACK_SCHEMA_VERSION - 1,
                "created_at": "2026-05-30T00:00:00Z",
                "state": test_state("core", Vec::new()),
                "artifacts": [],
            }))
            .expect("serialize previous rollback schema"),
        )
        .expect("write previous rollback schema");

        assert!(matches!(
            load_rollback_snapshot(&root).expect_err("previous schema should be invalid"),
            StateError::Parse(_) | StateError::InvalidRollback(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_preserves_live_namespace_replacement() {
        let root = test_root("cleanup-final-window-replacement");
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));

        quarantine_remove_file_with_hook(&target, &digest, 64, |_| {
            fs::write(&target, b"replacement").expect("write replacement");
        })
        .expect("remove parked owned target");

        assert_eq!(
            fs::read(&target).expect("replacement preserved"),
            b"replacement"
        );
        assert_eq!(
            inspect_cleanup_quarantine(&root).expect("inspect empty quarantine"),
            0
        );
        prove_managed_storage_recovered(&root, None).expect("prove empty quarantine");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_supports_an_anchored_instance_root() {
        let root = test_root("cleanup-anchored-root");
        fs::write(root.join("managed.jar"), b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));
        let anchor = AnchoredDirectory::open(&root).expect("anchor instance root");
        let target = anchor.path().join("managed.jar");

        quarantine_remove_file_with_hook(&target, &digest, 64, |_| {})
            .expect("remove through anchored root");

        assert!(!root.join("managed.jar").exists());
        assert_eq!(
            inspect_cleanup_quarantine(anchor.path()).expect("inspect anchored quarantine"),
            0
        );
        drop(anchor);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_restart_residue_latches_before_every_effect() {
        let root = test_root("cleanup-restart-unowned-entry");
        let quarantine = cleanup_quarantine_path(&root);
        ensure_cleanup_quarantine(&root).expect("create reserved quarantine");
        let bytes = b"self-consistent-but-unowned";
        let digest = hex::encode(Sha512::digest(bytes));
        let injected = quarantine.join(format!("{digest}.{:032x}.park", 1));
        fs::write(&injected, bytes).expect("write injected entry");

        assert!(reconcile_cleanup_quarantine(&root).is_err());
        assert!(reconcile_cleanup_quarantine(&root).is_err());

        assert_eq!(fs::read(&injected).expect("injected entry retained"), bytes);
        let preflight =
            preflight_managed_inspection_reconciliation(&root).expect("preflight retained entry");
        assert!(preflight.cleanup_quarantine);
        assert!(prove_managed_storage_recovered(&root, None).is_err());

        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write live target");
        let target_digest = hex::encode(Sha512::digest(b"owned"));
        assert!(quarantine_remove_file_with_hook(&target, &target_digest, 64, |_| {}).is_err());
        assert!(save_rollback_snapshot(&root, &test_state("core", Vec::new())).is_err());
        assert_eq!(fs::read(&target).expect("live target remains"), b"owned");
        assert_eq!(fs::read(&injected).expect("residue remains"), bytes);
        assert!(!rollback_dir_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_repeated_success_remains_empty() {
        let root = test_root("cleanup-success-plateau");
        let target = root.join("managed.jar");
        let digest = hex::encode(Sha512::digest(b"owned"));

        for _ in 0..64 {
            fs::write(&target, b"owned").expect("write owned target");
            quarantine_remove_file_with_hook(&target, &digest, 64, |_| {})
                .expect("settle owned target");
            assert!(!target.exists());
            assert_eq!(
                inspect_cleanup_quarantine(&root).expect("inspect successful cleanup"),
                0
            );
        }
        prove_managed_storage_recovered(&root, None).expect("prove plateaued cleanup");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_destination_collision_preserves_both_entries() {
        let root = test_root("cleanup-destination-collision");
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));
        let collision = std::cell::RefCell::new(None);

        let error = quarantine_remove_file_with_parking_hook(&target, &digest, 64, |parked| {
            fs::write(parked, b"collision").expect("write destination collision");
            *collision.borrow_mut() = parked.file_name().map(ToOwned::to_owned);
        })
        .expect_err("create-only parking must reject a collision");
        let collision =
            cleanup_quarantine_path(&root).join(collision.into_inner().expect("collision name"));

        assert!(matches!(error, StateError::Read(_)));
        assert_eq!(fs::read(&target).expect("source retained"), b"owned");
        assert_eq!(
            fs::read(&collision).expect("collision retained"),
            b"collision"
        );
        assert!(reconcile_cleanup_quarantine(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_live_source_replacement_before_move_fails_closed() {
        let root = test_root("cleanup-live-source-replacement");
        let target = root.join("managed.jar");
        let displaced = root.join("original.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));

        let error = quarantine_remove_file_with_parking_hook(&target, &digest, 64, |_| {
            fs::rename(&target, &displaced).expect("displace admitted live source");
            fs::write(&target, b"replacement").expect("write live replacement");
        })
        .expect_err("source replacement before move must fail closed");

        assert!(matches!(error, StateError::InvalidState(_)));
        assert_eq!(
            fs::read(&target).expect("live replacement remains in place"),
            b"replacement"
        );
        assert_eq!(
            fs::read(&displaced).expect("original admitted bytes remain reachable"),
            b"owned"
        );
        assert_eq!(
            inspect_cleanup_quarantine(&root).expect("inspect empty quarantine"),
            0
        );
        reconcile_cleanup_quarantine(&root).expect("empty quarantine remains settled");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_rejects_concurrent_residue_after_settlement() {
        let root = test_root("cleanup-post-settlement-residue");
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));
        let residue_digest = hex::encode(Sha512::digest(b"residue"));
        let residue_name = format!("{residue_digest}.{}.park", "f".repeat(32));

        let error = quarantine_remove_file_with_settlement_hook(&target, &digest, 64, |parked| {
            fs::write(
                parked
                    .parent()
                    .expect("parked file has quarantine parent")
                    .join(&residue_name),
                b"residue",
            )
            .expect("inject concurrent canonical residue");
        })
        .expect_err("whole-quarantine settlement proof must reject residue");
        let residue = cleanup_quarantine_path(&root).join(&residue_name);

        assert!(matches!(error, StateError::InvalidState(_)));
        assert!(!target.exists());
        assert_eq!(fs::read(&residue).expect("residue retained"), b"residue");
        assert!(reconcile_cleanup_quarantine(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_structural_bound_blocks_before_live_target_move() {
        let root = test_root("cleanup-structural-bound");
        let quarantine = cleanup_quarantine_path(&root);
        ensure_cleanup_quarantine(&root).expect("create reserved quarantine");
        let digest = hex::encode(Sha512::digest(b""));
        for index in 0..=RECOVERY_ENTRY_LIMIT {
            fs::write(quarantine.join(format!("{digest}.{index:032x}.park")), b"")
                .expect("write retained entry");
        }
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let target_digest = hex::encode(Sha512::digest(b"owned"));

        let error = quarantine_remove_file_with_hook(&target, &target_digest, 64, |_| {})
            .expect_err("out-of-bounds quarantine must block before parking");

        assert!(matches!(error, StateError::InvalidState(_)));
        assert_eq!(fs::read(&target).expect("live target remains"), b"owned");
        assert_eq!(
            fs::read_dir(&quarantine).expect("read quarantine").count(),
            RECOVERY_ENTRY_LIMIT + 1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_retained_handle_ignores_same_bytes_path_substitution() {
        let root = test_root("cleanup-same-bytes-window-replacement");
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));
        let observed = std::cell::RefCell::new(None);

        let result = quarantine_remove_file_with_settlement_hook(&target, &digest, 64, |parked| {
            let displaced = parked.with_extension("displaced");
            fs::rename(parked, &displaced).expect("displace admitted entry");
            fs::write(parked, b"owned").expect("write same-bytes replacement");
            *observed.borrow_mut() = Some((
                parked.file_name().expect("parked name").to_owned(),
                displaced.file_name().expect("displaced name").to_owned(),
            ));
        });
        let (parked, displaced) = observed.into_inner().expect("settlement names");
        let quarantine = cleanup_quarantine_path(&root);
        let parked = quarantine.join(parked);
        let displaced = quarantine.join(displaced);

        #[cfg(unix)]
        {
            assert_eq!(
                fs::read(displaced).expect("admitted bytes retained"),
                b"owned"
            );
        }
        #[cfg(windows)]
        assert!(!displaced.exists());
        assert!(matches!(&result, Err(StateError::InvalidState(_))));
        assert_eq!(
            fs::read(&parked).expect("unknown replacement preserved"),
            b"owned"
        );
        assert!(reconcile_cleanup_quarantine(&root).is_err());
        assert_eq!(
            fs::read(parked).expect("unknown replacement still preserved"),
            b"owned"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_quarantine_retains_identity_mismatch_for_fail_closed_recovery() {
        let root = test_root("cleanup-parked-mismatch");
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));

        let error = quarantine_remove_file_with_hook(&target, &digest, 64, |parked| {
            fs::rename(parked, parked.with_extension("displaced"))
                .expect("retain displaced admitted bytes");
            fs::write(parked, b"replacement").expect("replace parked pathname");
        })
        .expect_err("parked replacement must fail closed");

        assert!(matches!(error, StateError::InvalidState(_)));
        assert!(!target.exists());
        let displaced = fs::read_dir(cleanup_quarantine_path(&root))
            .expect("read retained entries")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "displaced")
            })
            .expect("displaced admitted entry")
            .path();
        assert_eq!(fs::read(displaced).expect("read admitted bytes"), b"owned");
        assert!(cleanup_quarantine_path(&root).is_dir());
        assert!(reconcile_cleanup_quarantine(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_quarantine_reproves_digest_after_same_inode_mutation() {
        let root = test_root("cleanup-same-inode-mutation");
        let target = root.join("managed.jar");
        fs::write(&target, b"owned").expect("write owned target");
        let digest = hex::encode(Sha512::digest(b"owned"));
        let parked = std::cell::RefCell::new(None);

        let error = quarantine_remove_file_with_settlement_hook(&target, &digest, 64, |path| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .expect("open admitted inode for mutation");
            file.write_all(b"other").expect("mutate admitted inode");
            file.sync_all().expect("sync mutated inode");
            *parked.borrow_mut() = path.file_name().map(ToOwned::to_owned);
        })
        .expect_err("same-inode digest mutation must fail closed");
        let parked = cleanup_quarantine_path(&root).join(parked.into_inner().expect("parked name"));

        assert!(matches!(error, StateError::InvalidState(_)));
        assert!(!target.exists());
        assert_eq!(fs::read(&parked).expect("mutated bytes retained"), b"other");
        assert!(reconcile_cleanup_quarantine(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_quarantine_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("cleanup-owner-only");
        ensure_cleanup_quarantine(&root).expect("create reserved quarantine");

        for path in [
            root.join(STATE_DIR_NAME),
            root.join(STATE_DIR_NAME).join(MUTATION_DIR_NAME),
            cleanup_quarantine_path(&root),
        ] {
            let mode = fs::symlink_metadata(path)
                .expect("reserved directory metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_empty_sha512() {
        let root = test_root("invalid-integrity");
        let mut state = test_state("core", vec![test_mod("sodium", "sodium.jar")]);
        state.installed_mods[0].integrity.sha512.clear();
        write_unvalidated_state(&root, state);

        let error = load_state(&root).expect_err("empty verified SHA-512 should be invalid");

        assert!(matches!(error, StateError::InvalidState(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_malformed_sha512_metadata() {
        let root = test_root("malformed-sha512");
        let mut state = test_state("core", vec![test_mod("sodium", "sodium.jar")]);
        state.installed_mods[0].integrity.sha512 = "abc123".to_string();
        write_unvalidated_state(&root, state);

        let error = load_state(&root).expect_err("short SHA-512 should be invalid");

        assert!(matches!(error, StateError::InvalidState(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_user_managed_artifacts_as_tracked_state() {
        let root = test_root("user-managed-state");
        let mut state = test_state("core", vec![test_mod("sodium", "user.jar")]);
        state.installed_mods[0].ownership_class = OwnershipClass::UserManaged;
        write_unvalidated_state(&root, state);

        let error = load_state(&root).expect_err("user-managed tracked state should fail");

        assert!(matches!(error, StateError::InvalidOwnership { .. }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_unversioned_state_without_rewriting_bytes() {
        let root = test_root("unversioned-state");
        let bytes =
            serde_json::to_vec(&test_state("core", Vec::new())).expect("serialize legacy state");
        fs::write(lock_file_path(&root), &bytes).expect("write legacy state");

        assert!(matches!(load_state(&root), Err(StateError::Parse(_))));
        assert_eq!(
            fs::read(lock_file_path(&root)).expect("read legacy state"),
            bytes
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn save_state_rejects_case_colliding_artifact_identities() {
        let root = test_root("case-colliding-state");
        let mut first = test_mod("Sodium", "sodium.jar");
        let mut second = test_mod("sodium", "SODIUM.JAR");
        first.integrity.sha512 = hex::encode(Sha512::digest(b"first"));
        second.integrity.sha512 = hex::encode(Sha512::digest(b"second"));

        let mut state = test_state("core", vec![first]);
        state.installed_mods.push(second);
        state.graph_sha512.clear();
        assert!(matches!(
            save_state(&root, &state),
            Err(StateError::InvalidState(_))
        ));
        assert!(!lock_file_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_state_rejects_oversized_and_nonregular_destinations_without_rewrite() {
        let oversized_root = test_root("oversized-state");
        let oversized_path = lock_file_path(&oversized_root);
        let oversized = fs::File::create(&oversized_path).expect("create oversized state");
        oversized
            .set_len(STATE_MAX_BYTES + 1)
            .expect("extend oversized state");
        drop(oversized);
        assert!(matches!(
            load_state(&oversized_root),
            Err(StateError::InvalidState(_))
        ));
        assert_eq!(
            fs::symlink_metadata(&oversized_path)
                .expect("oversized metadata")
                .len(),
            STATE_MAX_BYTES + 1
        );

        let directory_root = test_root("directory-state");
        fs::create_dir(lock_file_path(&directory_root)).expect("create state directory");
        assert!(matches!(
            load_state(&directory_root),
            Err(StateError::InvalidState(_))
        ));
        assert!(lock_file_path(&directory_root).is_dir());

        let _ = fs::remove_dir_all(oversized_root);
        let _ = fs::remove_dir_all(directory_root);
    }

    #[cfg(unix)]
    #[test]
    fn load_state_rejects_symlink_destination_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink-state");
        let outside = root.join("outside.json");
        let bytes = serde_json::to_vec(&PersistedCompositionState {
            schema_version: STATE_SCHEMA_VERSION,
            state: test_state("core", Vec::new()),
        })
        .expect("serialize outside state");
        fs::write(&outside, &bytes).expect("write outside state");
        symlink(&outside, lock_file_path(&root)).expect("link state destination");

        assert!(matches!(
            load_state(&root),
            Err(StateError::InvalidState(_))
        ));
        assert_eq!(fs::read(outside).expect("outside state remains"), bytes);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_reconciliation_preserves_user_replacement_and_exact_backup() {
        let root = test_root("removal-user-replacement");
        fs::write(root.join("managed-a.jar"), b"managed-a").expect("write managed artifact");
        let installed = test_mod("sodium", "managed-a.jar");
        save_state(&root, &test_state("core-a", vec![installed.clone()]))
            .expect("save managed state");
        let backup = stage_managed_artifact_removal(&root, &installed)
            .expect("stage managed artifact removal");
        fs::write(root.join("managed-a.jar"), b"user-replacement").expect("write user replacement");

        let error = load_state(&root).expect_err("conflicting replacement must block recovery");

        assert!(matches!(error, StateError::InvalidIntegrity { .. }));
        assert_eq!(
            fs::read(root.join("managed-a.jar")).expect("read user replacement"),
            b"user-replacement"
        );
        assert_eq!(
            fs::read(backup).expect("read retained managed backup"),
            b"managed-a"
        );
        let _ = fs::remove_dir_all(root);
    }

    fn test_state(composition_id: &str, installed_mods: Vec<InstalledMod>) -> CompositionState {
        let mut state = CompositionState {
            composition_id: composition_id.to_string(),
            family: VersionFamily::F,
            tier: CompositionTier::Core,
            game_version: "1.21.11".to_string(),
            loader: "fabric".to_string(),
            graph_sha512: String::new(),
            dependency_edges: Vec::new(),
            installed_mods,
            installed_at: "2026-05-30T00:00:00Z".to_string(),
        };
        state
            .installed_mods
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
        state.graph_sha512 = crate::install::plan::canonical_state_graph_digest(&state)
            .expect("canonical test graph");
        state
    }

    fn test_snapshot_candidate(
        root: &Path,
        state: &CompositionState,
        snapshot_id: &str,
    ) -> (PlannedRollbackSnapshot, RollbackSnapshot) {
        let planned = plan_rollback_artifacts(root, state, snapshot_id).expect("plan snapshot");
        let snapshot = RollbackSnapshot {
            id: snapshot_id.to_string(),
            schema_version: ROLLBACK_SCHEMA_VERSION,
            created_at: "2026-05-30T00:00:00Z".to_string(),
            target: RollbackSnapshotState::ManagedComposition {
                state: state.clone(),
            },
            artifacts: planned
                .artifacts
                .iter()
                .map(|artifact| artifact.metadata.clone())
                .collect(),
        };
        (planned, snapshot)
    }

    fn state_fixture(state: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "schema_version": STATE_SCHEMA_VERSION,
            "state": state,
        })
    }

    fn write_unvalidated_state(root: &Path, state: CompositionState) {
        fs::write(
            lock_file_path(root),
            serde_json::to_vec(&PersistedCompositionState {
                schema_version: STATE_SCHEMA_VERSION,
                state,
            })
            .expect("serialize invalid state"),
        )
        .expect("write invalid state");
    }

    fn test_mod(project_id: &str, filename: &str) -> InstalledMod {
        let bytes = filename
            .strip_suffix(".jar")
            .unwrap_or("managed")
            .as_bytes();
        InstalledMod {
            project_id: test_project_id(project_id),
            version_id: "NFkjnzWE".to_string(),
            filename: filename.to_string(),
            role: ManagedArtifactRole::Root,
            size: bytes.len() as u64,
            ownership_class: OwnershipClass::CompositionManaged,
            source: ManagedArtifactSource {
                provider: ManagedArtifactProvider::Modrinth,
            },
            integrity: ManagedArtifactIntegrity {
                sha512: hex::encode(Sha512::digest(bytes)),
            },
        }
    }

    fn test_project_id(label: &str) -> String {
        match label {
            "sodium" => "AANobbMI".to_string(),
            "lithium" => "gvQqBUqZ".to_string(),
            "ferrite" => "uXXizFIs".to_string(),
            value if value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) => {
                value.to_string()
            }
            value => hex::encode(Sha512::digest(value.as_bytes()))[..8].to_string(),
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
