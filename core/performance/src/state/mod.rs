use crate::MANAGED_ARTIFACT_MAX_BYTES;
use crate::storage::{
    ManagedStorageDirectory, ManagedStorageFile, retain_parked_file_after,
    settle_parked_directory_removal, settle_parked_file_removal, settle_parked_file_restore,
};
use crate::types::{CompositionState, CompositionTier, InstalledMod, OwnershipClass};
use axial_fs::{DirectoryEntry, DirectoryListingState, EntryKind};
use axial_minecraft::portable_path::{PortableFileName, PortablePathKey};
use chrono::Utc;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

const LOCK_FILE_NAME: &str = ".axial-lock.json";
const LOCK_STAGED_FILE_NAME: &str = ".axial-lock.json.new.tmp";
const LOCK_BACKUP_FILE_NAME: &str = ".axial-lock.json.previous.tmp";
const LOCK_DELETE_MARKER_NAME: &str = ".axial-lock.json.delete.intent";
const LOCK_DELETE_PARK_NAME: &str = ".axial-lock.json.delete.park";
const LOCK_DELETE_MARKER: &[u8] = b"axial-performance-state-delete-v2\n";
const STATE_SCHEMA_VERSION: i32 = 2;
const STATE_MAX_BYTES: u64 = 1024 * 1024;
const STATE_MAX_INSTALLED_MODS: usize = 256;
const STATE_TOKEN_MAX_CHARS: usize = 256;
const STATE_TIMESTAMP_MAX_CHARS: usize = 64;
pub(crate) const STATE_DIR_NAME: &str = ".axial-performance";
const MUTATION_DIR_NAME: &str = "mutations";
const REMOVAL_DIR_NAME: &str = "removals";
const ADDITION_DIR_NAME: &str = "additions";
const QUARANTINE_DIR_NAME: &str = "quarantine";
const ROLLBACK_DIR_NAME: &str = "rollback";
const ROLLBACK_HISTORY_DIR_NAME: &str = "history";
const ROLLBACK_TMP_DIR_NAME: &str = "tmp";
const ROLLBACK_METADATA_FILE_NAME: &str = "snapshot.json";
const ROLLBACK_CANDIDATE_PREFIX: &str = "candidate-";
const ROLLBACK_CANDIDATE_DELETE_PREFIX: &str = ".delete-candidate-";
const ROLLBACK_DELETE_PREFIX: &str = ".delete-";
const EMPTY_DIRECTORY_PARK_PREFIX: &str = ".park-";
const ROLLBACK_SCHEMA_VERSION: i32 = 4;
const ROLLBACK_HISTORY_LIMIT: usize = 5;
const ROLLBACK_METADATA_MAX_BYTES: u64 = 1024 * 1024;
const ROLLBACK_RETAINED_MAX_BYTES: u64 = MANAGED_ARTIFACT_MAX_BYTES * 2;
const ROLLBACK_TRANSIENT_MAX_BYTES: u64 =
    ROLLBACK_RETAINED_MAX_BYTES + MANAGED_ARTIFACT_MAX_BYTES + ROLLBACK_METADATA_MAX_BYTES;
pub(crate) const RECOVERY_ENTRY_LIMIT: usize = 1024;
const ADDITION_MARKER_SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to read state: {0}")]
    Read(#[from] io::Error),
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
    #[error("rollback snapshot candidate could not be completed and was discarded")]
    RollbackCandidateUnresumable,
    #[error("invalid performance state: {0}")]
    InvalidState(String),
    #[error("performance state publication failed during {phase}: {source}")]
    Publication {
        phase: StatePublicationPhase,
        #[source]
        source: io::Error,
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

struct AdmittedFile {
    file: ManagedStorageFile,
    bytes: Vec<u8>,
    sha256: [u8; 32],
    sha512: String,
}

struct AdmittedPersistedCompositionState {
    snapshot: PersistedCompositionState,
    file: ManagedStorageFile,
    sha256: [u8; 32],
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
    pub size: u64,
    pub sha512: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdditionMarker {
    schema_version: i32,
    artifact: InstalledMod,
}

enum RollbackCandidateCompletionError {
    Unresumable(StateError),
    Other(StateError),
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RollbackRestoreFaultPoint {
    BeforeStatePublication,
    AfterStatePublication,
}

#[cfg(test)]
thread_local! {
    static ROLLBACK_RESTORE_FAULT: std::cell::Cell<Option<RollbackRestoreFaultPoint>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn inject_rollback_restore_fault(point: RollbackRestoreFaultPoint) -> Result<(), StateError> {
    let injected = ROLLBACK_RESTORE_FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        Err(StateError::InvalidState(
            "injected rollback restore failure".to_string(),
        ))
    } else {
        Ok(())
    }
}

impl From<StateError> for RollbackCandidateCompletionError {
    fn from(error: StateError) -> Self {
        Self::Other(error)
    }
}

impl From<io::Error> for RollbackCandidateCompletionError {
    fn from(error: io::Error) -> Self {
        Self::Other(StateError::Read(error))
    }
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

impl RollbackSnapshotState {
    fn state(&self) -> Option<&CompositionState> {
        match self {
            Self::ManagedStateAbsent => None,
            Self::ManagedComposition { state } => Some(state),
        }
    }
}

pub(crate) fn load_state(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Option<CompositionState>, StateError> {
    reconcile_managed_storage(instance_mods)?;
    load_state_admitted(instance_mods)
}

pub(crate) fn reconcile_managed_storage(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    instance_mods.settle_pending_effects()?;
    reconcile_cleanup_quarantine(instance_mods)?;
    reconcile_state_publication(instance_mods)?;
    let state = load_state_admitted(instance_mods)?;
    reconcile_managed_addition_obligations(instance_mods, state.as_ref())?;
    reconcile_managed_removal_obligations(instance_mods, state.as_ref())?;
    reconcile_rollback_metadata(instance_mods)
}

pub(crate) fn managed_effect_reconciliation_required(
    instance_mods: &ManagedStorageDirectory,
) -> bool {
    instance_mods.effect_owner().has_pending()
}

pub(crate) fn recover_managed_storage(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Option<CompositionState>, StateError> {
    reconcile_managed_storage(instance_mods)?;
    let state = load_state_admitted(instance_mods)?;
    prove_managed_storage_recovered(instance_mods, state.as_ref())?;
    Ok(state)
}

pub(crate) fn prove_managed_storage_recovered(
    instance_mods: &ManagedStorageDirectory,
    state: Option<&CompositionState>,
) -> Result<(), StateError> {
    for name in [
        LOCK_STAGED_FILE_NAME,
        LOCK_BACKUP_FILE_NAME,
        LOCK_DELETE_MARKER_NAME,
        LOCK_DELETE_PARK_NAME,
    ] {
        if instance_mods.open_file_if_present(Path::new(name))?.is_some() {
            return Err(StateError::InvalidState(
                "managed state publication obligation remains after recovery".to_string(),
            ));
        }
    }
    if managed_addition_reconciliation_required(instance_mods)?
        || managed_removal_reconciliation_required(instance_mods)?
        || rollback_publication_reconciliation_required(instance_mods)?
        || cleanup_quarantine_reconciliation_required(instance_mods)?
    {
        return Err(StateError::InvalidState(
            "managed storage obligation remains after recovery".to_string(),
        ));
    }
    for installed in state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        if !managed_artifact_matches(instance_mods, installed)? {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "tracked managed artifact is missing or does not match its ownership digest"
                    .to_string(),
            });
        }
    }
    Ok(())
}

pub(crate) fn load_state_admitted(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Option<CompositionState>, StateError> {
    Ok(read_state_snapshot_if_present(instance_mods, LOCK_FILE_NAME)?
        .map(|snapshot| snapshot.snapshot.state))
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
    instance_mods: &ManagedStorageDirectory,
) -> Result<ManagedInspectionReconciliation, StateError> {
    Ok(ManagedInspectionReconciliation {
        state_publication: state_publication_reconciliation_required(instance_mods)?,
        managed_addition: managed_addition_reconciliation_required(instance_mods)?,
        managed_removal: managed_removal_reconciliation_required(instance_mods)?,
        rollback_publication: rollback_publication_reconciliation_required(instance_mods)?,
        cleanup_quarantine: cleanup_quarantine_reconciliation_required(instance_mods)?,
    })
}

pub(crate) fn reconcile_managed_inspection_publication(
    instance_mods: &ManagedStorageDirectory,
    preflight: ManagedInspectionReconciliation,
) -> Result<(), StateError> {
    if preflight.cleanup_quarantine {
        reconcile_cleanup_quarantine(instance_mods)?;
    }
    if preflight.state_publication {
        reconcile_state_publication(instance_mods)?;
    }
    Ok(())
}

pub(crate) fn reconcile_managed_inspection_obligations(
    instance_mods: &ManagedStorageDirectory,
    preflight: ManagedInspectionReconciliation,
    state: Option<&CompositionState>,
) -> Result<(), StateError> {
    if preflight.managed_addition {
        reconcile_managed_addition_obligations(instance_mods, state)?;
    }
    if preflight.managed_removal {
        reconcile_managed_removal_obligations(instance_mods, state)?;
    }
    if preflight.rollback_publication {
        reconcile_rollback_metadata(instance_mods)?;
    }
    Ok(())
}

pub(crate) fn save_state(
    instance_mods: &ManagedStorageDirectory,
    state: &CompositionState,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    validate_state(state)?;
    reconcile_state_publication_for_mutation(instance_mods)?;
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
    instance_mods
        .create_file_create_new(Path::new(LOCK_STAGED_FILE_NAME), &data)
        .map_err(|source| publication(StatePublicationPhase::Stage, source))?;
    publish_staged_state(instance_mods)
}

pub(crate) fn remove_state(instance_mods: &ManagedStorageDirectory) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    reconcile_state_publication_for_mutation(instance_mods)?;
    let Some(destination) = read_state_snapshot_if_present(instance_mods, LOCK_FILE_NAME)? else {
        return Ok(());
    };
    instance_mods
        .create_file_create_new(Path::new(LOCK_DELETE_MARKER_NAME), LOCK_DELETE_MARKER)
        .map_err(|source| publication(StatePublicationPhase::Stage, source))?;
    let parked = instance_mods
        .park_file_as(
            destination.file,
            LOCK_DELETE_PARK_NAME,
            destination.sha256,
        )
        .map_err(|source| publication(StatePublicationPhase::Backup, source))?;
    settle_parked_file_removal(instance_mods, parked)
        .map_err(|source| publication(StatePublicationPhase::Cleanup, source))?;
    quarantine_remove_exact(
        instance_mods,
        Path::new(LOCK_DELETE_MARKER_NAME),
        &hex::encode(Sha512::digest(LOCK_DELETE_MARKER)),
        LOCK_DELETE_MARKER.len() as u64,
    )
}

fn state_publication_reconciliation_required(
    instance_mods: &ManagedStorageDirectory,
) -> Result<bool, StateError> {
    for name in [
        LOCK_STAGED_FILE_NAME,
        LOCK_BACKUP_FILE_NAME,
        LOCK_DELETE_MARKER_NAME,
        LOCK_DELETE_PARK_NAME,
    ] {
        if instance_mods.open_file_if_present(Path::new(name))?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn publication(phase: StatePublicationPhase, source: io::Error) -> StateError {
    StateError::Publication { phase, source }
}

fn publish_staged_state(instance_mods: &ManagedStorageDirectory) -> Result<(), StateError> {
    let staged = read_state_snapshot_file(instance_mods, LOCK_STAGED_FILE_NAME)?;
    let backup = match read_state_snapshot_if_present(instance_mods, LOCK_FILE_NAME)? {
        Some(destination) => Some(
            instance_mods
                .park_file_for_restore_as(
                    destination.file,
                    LOCK_BACKUP_FILE_NAME,
                    destination.sha256,
                )
                .map_err(|source| publication(StatePublicationPhase::Backup, source))?,
        ),
        None => None,
    };
    let moved = instance_mods.move_file_no_replace(
        staged.file,
        Path::new(LOCK_FILE_NAME),
    );
    if let Err(source) = moved {
        if let Some(backup) = backup {
            settle_parked_file_restore(instance_mods, backup)
                .map_err(|restore| publication(StatePublicationPhase::Reconcile, restore))?;
        }
        return Err(publication(StatePublicationPhase::Publish, source));
    }
    if let Err(source) = instance_mods.sync() {
        let source = match backup {
            Some(backup) => retain_parked_file_after(instance_mods, backup),
            None => source,
        };
        return Err(publication(StatePublicationPhase::Publish, source));
    }
    if let Some(backup) = backup {
        settle_parked_file_removal(instance_mods, backup)
            .map_err(|source| publication(StatePublicationPhase::Cleanup, source))?;
    }
    Ok(())
}

pub(crate) fn reconcile_state_publication(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    let marker = read_bounded_file_if_present(
        instance_mods,
        Path::new(LOCK_DELETE_MARKER_NAME),
        LOCK_DELETE_MARKER.len() as u64,
    )?;
    if let Some(marker) = marker {
        if marker.bytes != LOCK_DELETE_MARKER {
            return Err(StateError::InvalidState(
                "performance state deletion marker ownership cannot be proven".to_string(),
            ));
        }
        if let Some(parked) = read_state_snapshot_if_present(instance_mods, LOCK_DELETE_PARK_NAME)? {
            let parked = instance_mods.admit_existing_file_park(
                LOCK_FILE_NAME,
                LOCK_DELETE_PARK_NAME,
                parked.sha256,
            )?;
            settle_parked_file_removal(instance_mods, parked)?;
        } else if let Some(destination) = read_state_snapshot_if_present(instance_mods, LOCK_FILE_NAME)? {
            let parked = instance_mods.park_file_as(
                destination.file,
                LOCK_DELETE_PARK_NAME,
                destination.sha256,
            )?;
            settle_parked_file_removal(instance_mods, parked)?;
        }
        if let Some(staged) = read_state_snapshot_if_present(instance_mods, LOCK_STAGED_FILE_NAME)? {
            quarantine_remove_exact(
                instance_mods,
                Path::new(LOCK_STAGED_FILE_NAME),
                &staged.sha512,
                staged.file.size(),
            )?;
        }
        quarantine_remove_exact(
            instance_mods,
            Path::new(LOCK_DELETE_MARKER_NAME),
            &marker.sha512,
            marker.file.size(),
        )?;
        return Ok(());
    }

    let destination = read_state_snapshot_if_present(instance_mods, LOCK_FILE_NAME)?;
    let staged = read_state_snapshot_if_present(instance_mods, LOCK_STAGED_FILE_NAME)?;
    let backup = read_state_snapshot_if_present(instance_mods, LOCK_BACKUP_FILE_NAME)?;
    let admitted_backup = backup
        .as_ref()
        .map(|backup| {
            instance_mods.admit_existing_file_park(
                LOCK_FILE_NAME,
                LOCK_BACKUP_FILE_NAME,
                backup.sha256,
            )
        })
        .transpose()?;
    match (destination, staged, admitted_backup) {
        (Some(_), Some(staged), Some(backup)) => {
            settle_parked_file_removal(instance_mods, backup)?;
            quarantine_remove_exact(
                instance_mods,
                Path::new(LOCK_STAGED_FILE_NAME),
                &staged.sha512,
                staged.file.size(),
            )?;
        }
        (Some(_), Some(staged), None) => {
            quarantine_remove_exact(
                instance_mods,
                Path::new(LOCK_STAGED_FILE_NAME),
                &staged.sha512,
                staged.file.size(),
            )?;
        }
        (Some(_), None, Some(backup)) => settle_parked_file_removal(instance_mods, backup)?,
        (None, Some(staged), Some(backup)) => {
            if let Err(move_error) =
                instance_mods.move_file_no_replace(staged.file, Path::new(LOCK_FILE_NAME))
            {
                settle_parked_file_restore(instance_mods, backup)?;
                return Err(StateError::Read(move_error));
            }
            settle_parked_file_removal(instance_mods, backup)?;
        }
        (None, Some(staged), None) => {
            instance_mods.move_file_no_replace(staged.file, Path::new(LOCK_FILE_NAME))?;
        }
        (None, None, Some(backup)) => {
            settle_parked_file_restore(instance_mods, backup)?;
        }
        (Some(_), None, None) | (None, None, None) => {}
    }
    Ok(())
}

fn reconcile_state_publication_for_mutation(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    reconcile_state_publication(instance_mods)?;
    load_state_admitted(instance_mods).map(|_| ())
}

fn read_state_snapshot_if_present(
    instance_mods: &ManagedStorageDirectory,
    name: &str,
) -> Result<Option<AdmittedPersistedCompositionState>, StateError> {
    let Some(admitted) = read_bounded_file_if_present(
        instance_mods,
        Path::new(name),
        STATE_MAX_BYTES,
    )? else {
        return Ok(None);
    };
    let snapshot = serde_json::from_slice::<PersistedCompositionState>(&admitted.bytes)?;
    validate_persisted_state(&snapshot)?;
    Ok(Some(AdmittedPersistedCompositionState {
        snapshot,
        file: admitted.file,
        sha256: admitted.sha256,
        sha512: admitted.sha512,
    }))
}

fn read_state_snapshot_file(
    instance_mods: &ManagedStorageDirectory,
    name: &str,
) -> Result<AdmittedPersistedCompositionState, StateError> {
    read_state_snapshot_if_present(instance_mods, name)?.ok_or_else(|| {
        StateError::InvalidState("performance state disappeared during admission".to_string())
    })
}

pub(crate) fn save_rollback_snapshot(
    instance_mods: &ManagedStorageDirectory,
    state: &CompositionState,
) -> Result<RollbackSnapshot, StateError> {
    validate_state(state)?;
    save_rollback_snapshot_target(
        instance_mods,
        RollbackSnapshotState::ManagedComposition {
            state: state.clone(),
        },
    )
}

pub(crate) fn save_absent_rollback_snapshot(
    instance_mods: &ManagedStorageDirectory,
) -> Result<RollbackSnapshot, StateError> {
    save_rollback_snapshot_target(instance_mods, RollbackSnapshotState::ManagedStateAbsent)
}

fn save_rollback_snapshot_target(
    instance_mods: &ManagedStorageDirectory,
    target: RollbackSnapshotState,
) -> Result<RollbackSnapshot, StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    reconcile_rollback_metadata(instance_mods)?;
    let id = new_rollback_snapshot_id();
    let artifacts = target
        .state()
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
        .enumerate()
        .map(|(index, installed)| RollbackArtifact {
            filename: installed.filename.clone(),
            stored_filename: format!("artifact-{index:03}.bin"),
            project_id: installed.project_id.clone(),
            version_id: installed.version_id.clone(),
            ownership_class: installed.ownership_class,
            size: installed.size,
            sha512: installed.integrity.sha512.to_ascii_lowercase(),
        })
        .collect::<Vec<_>>();
    let snapshot = RollbackSnapshot {
        id: id.clone(),
        schema_version: ROLLBACK_SCHEMA_VERSION,
        created_at: Utc::now().to_rfc3339(),
        target,
        artifacts,
    };
    validate_rollback_snapshot(&snapshot)?;
    let metadata = serde_json::to_vec_pretty(&snapshot)?;
    if metadata.len() as u64 > ROLLBACK_METADATA_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback metadata exceeds the byte budget".to_string(),
        ));
    }
    let candidate_bytes = rollback_snapshot_storage_bytes(&snapshot, metadata.len() as u64)?;
    let retained_bytes = retained_rollback_storage_bytes(instance_mods)?;
    if !matches!(
        retained_bytes.checked_add(candidate_bytes),
        Some(total) if total <= ROLLBACK_TRANSIENT_MAX_BYTES
    ) {
        return Err(StateError::InvalidRollback(
            "rollback storage exceeds the transient byte budget".to_string(),
        ));
    }

    let rollback = instance_mods.open_or_create_relative_directory(Path::new(&format!(
        "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}"
    )))?;
    let tmp = rollback.open_or_create_child(ROLLBACK_TMP_DIR_NAME)?;
    let history = rollback.open_or_create_child(ROLLBACK_HISTORY_DIR_NAME)?;
    let candidate_name = format!("{ROLLBACK_CANDIDATE_PREFIX}{id}");
    let candidate = tmp.open_or_create_child(&candidate_name)?;
    if !complete_entries(&candidate, "rollback candidate")?.is_empty() {
        return Err(StateError::InvalidRollback(
            "rollback candidate namespace is not empty".to_string(),
        ));
    }
    candidate.create_file_create_new(Path::new(ROLLBACK_METADATA_FILE_NAME), &metadata)?;
    candidate.sync()?;
    match complete_rollback_candidate(instance_mods, &candidate, &snapshot) {
        Ok(()) => {}
        Err(RollbackCandidateCompletionError::Unresumable(error)) => {
            discard_unresumable_rollback_candidate(
                instance_mods,
                &tmp,
                &candidate_name,
                candidate,
                &snapshot,
            )?;
            return Err(error);
        }
        Err(RollbackCandidateCompletionError::Other(error)) => return Err(error),
    }
    instance_mods.move_child_directory_no_replace(candidate, &history, &id)?;
    history.sync()?;
    prune_rollback_history(instance_mods)?;
    Ok(snapshot)
}

pub(crate) async fn save_rollback_snapshot_async(
    instance_mods: &ManagedStorageDirectory,
    state: &CompositionState,
) -> Result<RollbackSnapshot, StateError> {
    let instance_mods = instance_mods.clone();
    let state = state.clone();
    tokio::task::spawn_blocking(move || save_rollback_snapshot(&instance_mods, &state))
        .await
        .map_err(|_| StateError::Read(io::Error::other("rollback snapshot task stopped")))?
}

pub(crate) async fn save_absent_rollback_snapshot_async(
    instance_mods: &ManagedStorageDirectory,
) -> Result<RollbackSnapshot, StateError> {
    let instance_mods = instance_mods.clone();
    tokio::task::spawn_blocking(move || save_absent_rollback_snapshot(&instance_mods))
        .await
        .map_err(|_| StateError::Read(io::Error::other("rollback snapshot task stopped")))?
}

pub(crate) fn load_rollback_snapshot(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Option<RollbackSnapshot>, StateError> {
    reconcile_managed_storage(instance_mods)?;
    load_rollback_snapshot_admitted(instance_mods)
}

pub(crate) fn load_rollback_snapshot_admitted(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Option<RollbackSnapshot>, StateError> {
    Ok(load_retained_rollback_snapshots(instance_mods)?
        .into_iter()
        .next()
        .map(|record| record.snapshot))
}

pub(crate) async fn load_rollback_snapshot_async(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Option<RollbackSnapshot>, StateError> {
    let instance_mods = instance_mods.clone();
    tokio::task::spawn_blocking(move || load_rollback_snapshot(&instance_mods))
        .await
        .map_err(|_| StateError::Read(io::Error::other("rollback load task stopped")))?
}

pub(crate) fn load_rollback_snapshot_by_id(
    instance_mods: &ManagedStorageDirectory,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    reconcile_managed_storage(instance_mods)?;
    load_rollback_snapshot_by_id_admitted(instance_mods, snapshot_id)
}

pub(crate) fn load_rollback_snapshot_by_id_admitted(
    instance_mods: &ManagedStorageDirectory,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    validate_rollback_snapshot_id(snapshot_id)?;
    let Some(history) = open_optional_directory(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}/{ROLLBACK_HISTORY_DIR_NAME}"
        )),
    )? else {
        return Ok(None);
    };
    let Some(directory) = history.open_child(snapshot_id)? else {
        return Ok(None);
    };
    read_snapshot_directory(&directory, snapshot_id).map(Some)
}

pub(crate) async fn load_rollback_snapshot_by_id_async(
    instance_mods: &ManagedStorageDirectory,
    snapshot_id: &str,
) -> Result<Option<RollbackSnapshot>, StateError> {
    let instance_mods = instance_mods.clone();
    let snapshot_id = snapshot_id.to_string();
    tokio::task::spawn_blocking(move || {
        load_rollback_snapshot_by_id(&instance_mods, &snapshot_id)
    })
    .await
    .map_err(|_| StateError::Read(io::Error::other("rollback history load task stopped")))?
}

pub(crate) fn list_rollback_snapshots_admitted(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Vec<RollbackSnapshotSummary>, StateError> {
    let records = load_retained_rollback_snapshots(instance_mods)?;
    Ok(records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            let state = record.snapshot.state();
            RollbackSnapshotSummary {
                id: record.snapshot.id.clone(),
                created_at: record.snapshot.created_at.clone(),
                target: record.snapshot.target_kind(),
                composition_id: state.map(|state| state.composition_id.clone()),
                tier: state.map(|state| state.tier),
                installed_count: state.map_or(0, |state| state.installed_mods.len()),
                artifact_count: record.snapshot.artifacts.len(),
                ownership_class: OwnershipClass::CompositionManaged,
                rollback_available: true,
                latest: index == 0,
            }
        })
        .collect())
}

pub(crate) fn restore_rollback_snapshot(
    instance_mods: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<ManagedRollbackOutcome, StateError> {
    restore_rollback_snapshot_classified(instance_mods, snapshot)
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
    instance_mods: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<ManagedRollbackOutcome, RollbackRestoreError> {
    require_cleanup_quarantine_empty(instance_mods).map_err(RollbackRestoreError::Definite)?;
    validate_rollback_snapshot(snapshot).map_err(RollbackRestoreError::Definite)?;
    let retained = load_rollback_snapshot_by_id(instance_mods, &snapshot.id)
        .map_err(RollbackRestoreError::Definite)?
        .ok_or_else(|| {
            RollbackRestoreError::Definite(StateError::InvalidRollback(
                "rollback snapshot is not retained".to_string(),
            ))
        })?;
    if retained != *snapshot {
        return Err(RollbackRestoreError::Definite(StateError::InvalidRollback(
            "rollback snapshot does not match retained metadata".to_string(),
        )));
    }
    let current = load_state(instance_mods).map_err(RollbackRestoreError::Definite)?;
    let result = restore_snapshot_graph(instance_mods, snapshot, current.as_ref());
    if let Err(error) = result {
        instance_mods
            .settle_pending_effects()
            .map_err(StateError::Read)
            .map_err(RollbackRestoreError::Indeterminate)?;
        reconcile_state_publication(instance_mods)
            .map_err(RollbackRestoreError::Indeterminate)?;
        let authoritative = load_state_admitted(instance_mods)
            .map_err(RollbackRestoreError::Indeterminate)?;
        reconcile_managed_addition_obligations(instance_mods, authoritative.as_ref())
            .map_err(RollbackRestoreError::Indeterminate)?;
        reconcile_managed_removal_obligations(instance_mods, authoritative.as_ref())
            .map_err(RollbackRestoreError::Indeterminate)?;
        return Err(RollbackRestoreError::Indeterminate(error));
    }
    Ok(match snapshot.state() {
        Some(state) => ManagedRollbackOutcome::ManagedComposition(state.clone()),
        None => ManagedRollbackOutcome::ManagedStateAbsent,
    })
}

pub(crate) async fn restore_rollback_snapshot_classified_async(
    instance_mods: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<ManagedRollbackOutcome, RollbackRestoreError> {
    let instance_mods = instance_mods.clone();
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || {
        restore_rollback_snapshot_classified(&instance_mods, &snapshot)
    })
    .await
    .map_err(|_| {
        RollbackRestoreError::Indeterminate(StateError::Read(io::Error::other(
            "rollback restore task stopped",
        )))
    })?
}

fn restore_snapshot_graph(
    instance_mods: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
    current: Option<&CompositionState>,
) -> Result<(), StateError> {
    let desired = snapshot
        .state()
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
        .map(|installed| (installed.filename.as_str(), installed))
        .collect::<HashMap<_, _>>();
    let current_artifacts = current
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
        .map(|installed| (installed.filename.as_str(), installed))
        .collect::<HashMap<_, _>>();

    for installed in current_artifacts.values() {
        let retained = desired
            .get(installed.filename.as_str())
            .is_some_and(|desired| same_artifact_identity(installed, desired))
            && managed_artifact_matches(instance_mods, installed)?;
        if !retained {
            stage_managed_artifact_removal(instance_mods, installed)?;
        }
    }

    let snapshot_directory = rollback_snapshot_directory(instance_mods, &snapshot.id)?;
    for artifact in &snapshot.artifacts {
        let desired_artifact = desired.get(artifact.filename.as_str()).ok_or_else(|| {
            StateError::InvalidRollback(format!(
                "rollback artifact {} has no target state entry",
                artifact.filename
            ))
        })?;
        let retained = current_artifacts
            .get(artifact.filename.as_str())
            .is_some_and(|current| same_artifact_identity(current, desired_artifact))
            && managed_artifact_matches(instance_mods, desired_artifact)?;
        if retained {
            continue;
        }
        if instance_mods
            .open_file_if_present(Path::new(&artifact.filename))?
            .is_some()
        {
            return Err(StateError::InvalidIntegrity {
                filename: artifact.filename.clone(),
                reason: "rollback destination is occupied by unowned content".to_string(),
            });
        }
        let source = snapshot_directory.open_file(Path::new(&artifact.stored_filename))?;
        if source.size() != artifact.size
            || !source
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&artifact.sha512)
        {
            return Err(StateError::InvalidRollback(format!(
                "rollback artifact {} changed before restore",
                artifact.filename
            )));
        }
        let obligation = prepare_managed_artifact_addition(instance_mods, desired_artifact)?;
        instance_mods.copy_file_create_new(
            &source,
            Path::new(&artifact.filename),
            MANAGED_ARTIFACT_MAX_BYTES,
        )?;
        publish_managed_artifact_addition(instance_mods, desired_artifact, &obligation)?;
    }
    #[cfg(test)]
    inject_rollback_restore_fault(RollbackRestoreFaultPoint::BeforeStatePublication)?;
    match snapshot.state() {
        Some(state) => save_state(instance_mods, state)?,
        None => remove_state(instance_mods)?,
    }
    #[cfg(test)]
    inject_rollback_restore_fault(RollbackRestoreFaultPoint::AfterStatePublication)?;
    reconcile_managed_addition_obligations(instance_mods, snapshot.state())?;
    for installed in current_artifacts.values() {
        if !desired
            .get(installed.filename.as_str())
            .is_some_and(|desired| same_artifact_identity(installed, desired))
        {
            settle_managed_artifact_removal(instance_mods, installed)?;
        }
    }
    Ok(())
}

pub(crate) fn managed_artifact_matches(
    instance_mods: &ManagedStorageDirectory,
    installed: &InstalledMod,
) -> Result<bool, StateError> {
    validate_managed_filename(&installed.filename)?;
    let Some(file) = instance_mods.open_file_if_present(Path::new(&installed.filename))? else {
        return Ok(false);
    };
    if file.size() != installed.size || file.size() > MANAGED_ARTIFACT_MAX_BYTES {
        return Ok(false);
    }
    Ok(file
        .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
        .eq_ignore_ascii_case(&installed.integrity.sha512))
}

pub(crate) fn stage_managed_artifact_removal(
    instance_mods: &ManagedStorageDirectory,
    installed: &InstalledMod,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    validate_managed_filename(&installed.filename)?;
    validate_sha512_integrity(&installed.filename, &installed.integrity.sha512)?;
    let removal_relative = removal_backup_relative(installed);
    if let Some(backup) = instance_mods.open_file_if_present(&removal_relative)? {
        if backup.size() != installed.size
            || !backup
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&installed.integrity.sha512)
        {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed removal backup ownership cannot be proven".to_string(),
            });
        }
        if instance_mods
            .open_file_if_present(Path::new(&installed.filename))?
            .is_some()
        {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed removal has both a retained backup and a live destination"
                    .to_string(),
            });
        }
        return Ok(());
    }
    let Some(source) = instance_mods.open_file_if_present(Path::new(&installed.filename))? else {
        return Ok(());
    };
    if source.size() != installed.size
        || !source
            .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
            .eq_ignore_ascii_case(&installed.integrity.sha512)
    {
        return Err(StateError::InvalidIntegrity {
            filename: installed.filename.clone(),
            reason: "current bytes do not match the recorded ownership digest".to_string(),
        });
    }
    instance_mods.move_file_no_replace(source, &removal_relative)?;
    Ok(())
}

pub(crate) struct ManagedArtifactAdditionObligation {
    marker_name: String,
    marker_sha512: String,
    artifact: InstalledMod,
    destination: ManagedStorageDirectory,
}

impl ManagedArtifactAdditionObligation {
    pub(crate) fn parent(&self) -> &ManagedStorageDirectory {
        &self.destination
    }

    pub(crate) fn filename(&self) -> &str {
        &self.artifact.filename
    }
}

pub(crate) fn prepare_managed_artifact_addition(
    instance_mods: &ManagedStorageDirectory,
    installed: &InstalledMod,
) -> Result<ManagedArtifactAdditionObligation, StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    validate_managed_filename(&installed.filename)?;
    validate_sha512_integrity(&installed.filename, &installed.integrity.sha512)?;
    if instance_mods
        .open_file_if_present(Path::new(&installed.filename))?
        .is_some()
    {
        return Err(StateError::InvalidIntegrity {
            filename: installed.filename.clone(),
            reason: "managed addition destination is already occupied".to_string(),
        });
    }
    let additions = instance_mods.open_or_create_relative_directory(Path::new(&format!(
        "{STATE_DIR_NAME}/{MUTATION_DIR_NAME}/{ADDITION_DIR_NAME}"
    )))?;
    let marker_name = addition_marker_name(installed);
    let marker = AdditionMarker {
        schema_version: ADDITION_MARKER_SCHEMA_VERSION,
        artifact: installed.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)?;
    if bytes.len() as u64 > STATE_MAX_BYTES {
        return Err(StateError::InvalidState(
            "managed addition marker exceeds the byte budget".to_string(),
        ));
    }
    let marker_sha512 = hex::encode(Sha512::digest(&bytes));
    additions.create_file_create_new(Path::new(&marker_name), &bytes)?;
    additions.sync()?;
    Ok(ManagedArtifactAdditionObligation {
        marker_name,
        marker_sha512,
        artifact: installed.clone(),
        destination: instance_mods.clone(),
    })
}

pub(crate) fn publish_managed_artifact_addition(
    instance_mods: &ManagedStorageDirectory,
    installed: &InstalledMod,
    obligation: &ManagedArtifactAdditionObligation,
) -> Result<(), StateError> {
    if obligation.artifact != *installed
        || obligation.marker_name != addition_marker_name(installed)
    {
        return Err(StateError::InvalidState(
            "managed addition obligation does not match its artifact".to_string(),
        ));
    }
    let marker_relative = addition_marker_relative(&obligation.marker_name);
    let marker = read_bounded_file(
        instance_mods,
        &marker_relative,
        STATE_MAX_BYTES,
    )?;
    if marker.sha512 != obligation.marker_sha512
        || serde_json::from_slice::<AdditionMarker>(&marker.bytes)?.artifact != *installed
    {
        return Err(StateError::InvalidState(
            "managed addition marker changed before publication".to_string(),
        ));
    }
    if !managed_artifact_matches(instance_mods, installed)? {
        return Err(StateError::InvalidIntegrity {
            filename: installed.filename.clone(),
            reason: "managed addition publication could not be proven".to_string(),
        });
    }
    Ok(())
}

pub(crate) fn reconcile_managed_addition_obligations(
    instance_mods: &ManagedStorageDirectory,
    current_state: Option<&CompositionState>,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    let Some(additions) = open_optional_directory(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{MUTATION_DIR_NAME}/{ADDITION_DIR_NAME}"
        )),
    )? else {
        return Ok(());
    };
    let entries = complete_entries(&additions, "managed addition obligations")?;
    let mut filenames = HashSet::new();
    for entry in entries {
        if entry.kind() != EntryKind::File {
            return Err(StateError::InvalidState(
                "managed addition obligation is not a regular file".to_string(),
            ));
        }
        let name = entry_name(&entry, "managed addition obligation")?;
        let marker_relative = addition_marker_relative(&name);
        let admitted = read_bounded_file(instance_mods, &marker_relative, STATE_MAX_BYTES)?;
        let marker = serde_json::from_slice::<AdditionMarker>(&admitted.bytes)?;
        validate_addition_marker(&name, &marker)?;
        let filename_key = portable_filename_key(&marker.artifact.filename)?;
        if !filenames.insert(filename_key) {
            return Err(StateError::InvalidState(
                "managed addition obligations contain colliding filenames".to_string(),
            ));
        }
        let tracked = current_state.is_some_and(|state| {
            state
                .installed_mods
                .iter()
                .any(|installed| same_artifact_identity(installed, &marker.artifact))
        });
        let final_file = instance_mods.open_file_if_present(Path::new(&marker.artifact.filename))?;
        if tracked {
            if !managed_artifact_matches(instance_mods, &marker.artifact)? {
                return Err(StateError::InvalidIntegrity {
                    filename: marker.artifact.filename.clone(),
                    reason: "tracked managed addition is missing or changed".to_string(),
                });
            }
        } else if let Some(final_file) = final_file {
            let exact = final_file.size() == marker.artifact.size
                && final_file
                    .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                    .eq_ignore_ascii_case(&marker.artifact.integrity.sha512);
            if exact {
                quarantine_remove_exact(
                    instance_mods,
                    Path::new(&marker.artifact.filename),
                    &marker.artifact.integrity.sha512,
                    marker.artifact.size,
                )?;
            }
        }
        quarantine_remove_exact(
            instance_mods,
            &marker_relative,
            &admitted.sha512,
            admitted.file.size(),
        )?;
    }
    Ok(())
}

pub(crate) fn settle_managed_artifact_removal(
    instance_mods: &ManagedStorageDirectory,
    installed: &InstalledMod,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    let relative = removal_backup_relative(installed);
    if let Some(backup) = instance_mods.open_file_if_present(&relative)? {
        if backup.size() != installed.size
            || !backup
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&installed.integrity.sha512)
        {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed removal backup ownership cannot be proven".to_string(),
            });
        }
        quarantine_remove_exact(
            instance_mods,
            &relative,
            &installed.integrity.sha512,
            installed.size,
        )?;
    }
    Ok(())
}

fn managed_addition_reconciliation_required(
    instance_mods: &ManagedStorageDirectory,
) -> Result<bool, StateError> {
    directory_has_entries(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{MUTATION_DIR_NAME}/{ADDITION_DIR_NAME}"
        )),
        "managed addition obligations",
    )
}

fn managed_removal_reconciliation_required(
    instance_mods: &ManagedStorageDirectory,
) -> Result<bool, StateError> {
    directory_has_entries(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{MUTATION_DIR_NAME}/{REMOVAL_DIR_NAME}"
        )),
        "managed removal obligations",
    )
}

pub(crate) fn reconcile_managed_removal_obligations(
    instance_mods: &ManagedStorageDirectory,
    current_state: Option<&CompositionState>,
) -> Result<(), StateError> {
    require_cleanup_quarantine_empty(instance_mods)?;
    let Some(removals) = open_optional_directory(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{MUTATION_DIR_NAME}/{REMOVAL_DIR_NAME}"
        )),
    )? else {
        return Ok(());
    };
    reconcile_empty_directory_parks(&removals, "managed removal cleanup")?;
    let digest_entries = complete_entries(&removals, "managed removal obligations")?;
    let mut seen = HashSet::new();
    for digest_entry in digest_entries {
        if digest_entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidState(
                "managed removal digest entry is not a directory".to_string(),
            ));
        }
        let digest = entry_name(&digest_entry, "managed removal digest")?;
        if !is_valid_sha512(&digest) {
            return Err(StateError::InvalidState(
                "managed removal digest is invalid".to_string(),
            ));
        }
        let digest_dir = removals.open_observed_child(&digest_entry)?;
        for file_entry in complete_entries(&digest_dir, "managed removal obligations")? {
            if file_entry.kind() != EntryKind::File {
                return Err(StateError::InvalidState(
                    "managed removal obligation is not a regular file".to_string(),
                ));
            }
            let filename = entry_name(&file_entry, "managed removal filename")?;
            let key = portable_filename_key(&filename)?;
            if !seen.insert(key) {
                return Err(StateError::InvalidState(
                    "managed removal obligations contain colliding filenames".to_string(),
                ));
            }
            let relative = removal_backup_relative_parts(&digest, &filename);
            let backup = instance_mods.open_file(&relative)?;
            if !backup
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&digest)
            {
                return Err(StateError::InvalidIntegrity {
                    filename,
                    reason: "managed removal obligation ownership cannot be proven".to_string(),
                });
            }
            let tracked = current_state.and_then(|state| {
                state.installed_mods.iter().find(|installed| {
                    installed.filename == filename
                        && installed.integrity.sha512.eq_ignore_ascii_case(&digest)
                })
            });
            if let Some(installed) = tracked {
                if let Some(live) = instance_mods.open_file_if_present(Path::new(&filename))? {
                    if live.size() != installed.size
                        || !live
                            .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                            .eq_ignore_ascii_case(&digest)
                    {
                        return Err(StateError::InvalidIntegrity {
                            filename,
                            reason: "managed removal destination conflicts with retained ownership"
                                .to_string(),
                        });
                    }
                    quarantine_remove_exact(
                        instance_mods,
                        &relative,
                        &digest,
                        backup.size(),
                    )?;
                } else {
                    instance_mods.move_file_no_replace(backup, Path::new(&filename))?;
                }
            } else {
                quarantine_remove_exact(instance_mods, &relative, &digest, backup.size())?;
            }
        }
        remove_empty_child(&removals, &digest)?;
    }
    Ok(())
}

pub(crate) fn require_cleanup_quarantine_empty(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    reconcile_cleanup_quarantine(instance_mods)?;
    if cleanup_quarantine_reconciliation_required(instance_mods)? {
        Err(StateError::InvalidState(
            "managed cleanup quarantine is not empty".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn quarantine_remove_exact(
    instance_mods: &ManagedStorageDirectory,
    relative: &Path,
    expected_sha512: &str,
    expected_size: u64,
) -> Result<(), StateError> {
    if !is_valid_sha512(expected_sha512) {
        return Err(StateError::InvalidIntegrity {
            filename: relative.to_string_lossy().into_owned(),
            reason: "cleanup digest is invalid".to_string(),
        });
    }
    let file = instance_mods.open_file(relative)?;
    let (sha256, sha512) = file.digests(expected_size.max(1))?;
    if file.size() != expected_size
        || !hex::encode(sha512).eq_ignore_ascii_case(expected_sha512)
    {
        return Err(StateError::InvalidIntegrity {
            filename: relative.to_string_lossy().into_owned(),
            reason: "cleanup ownership cannot be proven".to_string(),
        });
    }
    let digest = expected_sha512.to_ascii_lowercase();
    let quarantine_root = instance_mods.open_or_create_relative_directory(Path::new(&format!(
        "{STATE_DIR_NAME}/{QUARANTINE_DIR_NAME}"
    )))?;
    let quarantine = quarantine_root.open_or_create_child(&digest)?;
    let parked_name = quarantine_leaf(relative)?;
    let moved_relative = PathBuf::from(STATE_DIR_NAME)
        .join(QUARANTINE_DIR_NAME)
        .join(&digest)
        .join(&parked_name);
    instance_mods.move_file_no_replace(file, &moved_relative)?;
    let parked = quarantine.admit_existing_file_park("discarded", &parked_name, sha256)?;
    settle_parked_file_removal(instance_mods, parked)?;
    remove_empty_child(&quarantine_root, &digest)?;
    Ok(())
}

fn cleanup_quarantine_reconciliation_required(
    instance_mods: &ManagedStorageDirectory,
) -> Result<bool, StateError> {
    directory_has_entries(
        instance_mods,
        Path::new(&format!("{STATE_DIR_NAME}/{QUARANTINE_DIR_NAME}")),
        "managed cleanup quarantine",
    )
}

fn reconcile_cleanup_quarantine(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    let Some(quarantine) = open_optional_directory(
        instance_mods,
        Path::new(&format!("{STATE_DIR_NAME}/{QUARANTINE_DIR_NAME}")),
    )? else {
        return Ok(());
    };
    reconcile_empty_directory_parks(&quarantine, "cleanup quarantine")?;
    let digest_entries = complete_entries(&quarantine, "managed cleanup quarantine")?;
    for digest_entry in digest_entries {
        if digest_entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidState(
                "cleanup quarantine digest entry is not a directory".to_string(),
            ));
        }
        let digest = entry_name(&digest_entry, "cleanup quarantine digest")?;
        if !is_valid_sha512(&digest) {
            return Err(StateError::InvalidState(
                "cleanup quarantine digest is invalid".to_string(),
            ));
        }
        let digest_dir = quarantine.open_observed_child(&digest_entry)?;
        let entries = complete_entries(&digest_dir, "managed cleanup quarantine")?;
        for entry in entries {
            if entry.kind() != EntryKind::File {
                return Err(StateError::InvalidState(
                    "cleanup quarantine entry is not a regular file".to_string(),
                ));
            }
            let name = entry_name(&entry, "cleanup quarantine file")?;
            let file = digest_dir.open_file(Path::new(&name))?;
            let (sha256, sha512) = file.digests(MANAGED_ARTIFACT_MAX_BYTES)?;
            if !hex::encode(sha512).eq_ignore_ascii_case(&digest) {
                return Err(StateError::InvalidIntegrity {
                    filename: "cleanup quarantine entry".to_string(),
                    reason: "parked cleanup ownership cannot be proven".to_string(),
                });
            }
            let parked = digest_dir.admit_existing_file_park("discarded", &name, sha256)?;
            settle_parked_file_removal(instance_mods, parked)?;
        }
        remove_empty_child(&quarantine, &digest)?;
    }
    Ok(())
}

pub(crate) fn reconcile_rollback_metadata(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    reconcile_cleanup_quarantine(instance_mods)?;
    let Some(rollback) = open_optional_directory(
        instance_mods,
        Path::new(&format!("{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}")),
    )? else {
        return Ok(());
    };
    let history = rollback.open_or_create_child(ROLLBACK_HISTORY_DIR_NAME)?;
    reconcile_deleted_snapshot_directories(instance_mods, &history)?;
    let tmp = rollback.open_or_create_child(ROLLBACK_TMP_DIR_NAME)?;
    reconcile_empty_directory_parks(&tmp, "rollback candidate cleanup")?;
    reconcile_deleted_rollback_candidates(instance_mods, &tmp)?;
    for entry in complete_entries(&tmp, "rollback candidates")? {
        if entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidRollback(
                "rollback candidate entry is not a directory".to_string(),
            ));
        }
        let candidate_name = entry_name(&entry, "rollback candidate")?;
        let id = candidate_name
            .strip_prefix(ROLLBACK_CANDIDATE_PREFIX)
            .ok_or_else(|| {
                StateError::InvalidRollback("rollback candidate name is invalid".to_string())
            })?;
        validate_rollback_snapshot_id(id)?;
        if history.open_child(id)?.is_some() {
            return Err(StateError::InvalidRollback(
                "rollback candidate conflicts with retained history".to_string(),
            ));
        }
        let candidate = tmp.open_observed_child(&entry)?;
        if complete_entries(&candidate, "rollback candidate")?.is_empty() {
            remove_empty_child(&tmp, &candidate_name)?;
            continue;
        }
        let snapshot = read_rollback_candidate(&candidate, id)?;
        let candidate_bytes = rollback_directory_storage_bytes(&candidate, &snapshot)?;
        let retained_bytes = retained_rollback_storage_bytes(instance_mods)?;
        if !matches!(
            retained_bytes.checked_add(candidate_bytes),
            Some(total) if total <= ROLLBACK_TRANSIENT_MAX_BYTES
        ) {
            return Err(StateError::InvalidRollback(
                "rollback recovery exceeds the transient byte budget".to_string(),
            ));
        }
        match complete_rollback_candidate(instance_mods, &candidate, &snapshot) {
            Ok(()) => {}
            Err(RollbackCandidateCompletionError::Unresumable(error)) => {
                discard_unresumable_rollback_candidate(
                    instance_mods,
                    &tmp,
                    &candidate_name,
                    candidate,
                    &snapshot,
                )?;
                return Err(error);
            }
            Err(RollbackCandidateCompletionError::Other(error)) => return Err(error),
        }
        instance_mods.move_child_directory_no_replace(candidate, &history, id)?;
        history.sync()?;
        prune_rollback_history(instance_mods)?;
    }
    Ok(())
}

fn rollback_publication_reconciliation_required(
    instance_mods: &ManagedStorageDirectory,
) -> Result<bool, StateError> {
    let tmp = directory_has_entries(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}/{ROLLBACK_TMP_DIR_NAME}"
        )),
        "rollback candidates",
    )?;
    let Some(history) = open_optional_directory(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}/{ROLLBACK_HISTORY_DIR_NAME}"
        )),
    )? else {
        return Ok(tmp);
    };
    let entries = complete_entries(&history, "rollback history")?;
    if tmp
        || entries.len() > ROLLBACK_HISTORY_LIMIT
        || entries.iter().any(|entry| {
            entry.utf8_name().is_some_and(|name| {
                name.starts_with(ROLLBACK_DELETE_PREFIX)
                    || name.starts_with(EMPTY_DIRECTORY_PARK_PREFIX)
            })
        })
    {
        return Ok(true);
    }
    let mut retained_bytes = 0_u64;
    for entry in entries {
        let id = entry_name(&entry, "rollback history id")?;
        validate_rollback_snapshot_id(&id)?;
        if entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidRollback(
                "rollback history entry is not a directory".to_string(),
            ));
        }
        let directory = history.open_observed_child(&entry)?;
        let (snapshot, metadata_bytes) = read_rollback_metadata(&directory, &id)?;
        let snapshot_bytes = rollback_snapshot_storage_bytes(&snapshot, metadata_bytes)?;
        retained_bytes = retained_bytes.checked_add(snapshot_bytes).ok_or_else(|| {
            StateError::InvalidRollback("rollback retained byte budget overflowed".to_string())
        })?;
        if retained_bytes > ROLLBACK_RETAINED_MAX_BYTES {
            return Ok(true);
        }
    }
    Ok(false)
}

struct RetainedRollbackSnapshot {
    snapshot: RollbackSnapshot,
    storage_bytes: u64,
}

fn load_retained_rollback_snapshots(
    instance_mods: &ManagedStorageDirectory,
) -> Result<Vec<RetainedRollbackSnapshot>, StateError> {
    let Some(history) = open_optional_directory(
        instance_mods,
        Path::new(&format!(
            "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}/{ROLLBACK_HISTORY_DIR_NAME}"
        )),
    )? else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    for entry in complete_entries(&history, "rollback history")? {
        if entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidRollback(
                "rollback history entry is not a directory".to_string(),
            ));
        }
        let id = entry_name(&entry, "rollback history id")?;
        if id.starts_with(ROLLBACK_DELETE_PREFIX)
            || id.starts_with(EMPTY_DIRECTORY_PARK_PREFIX)
        {
            return Err(StateError::InvalidRollback(
                "rollback history deletion remains unsettled".to_string(),
            ));
        }
        validate_rollback_snapshot_id(&id)?;
        let directory = history.open_observed_child(&entry)?;
        let snapshot = read_snapshot_directory(&directory, &id)?;
        let storage_bytes = rollback_directory_storage_bytes(&directory, &snapshot)?;
        records.push(RetainedRollbackSnapshot {
            snapshot,
            storage_bytes,
        });
    }
    records.sort_by(|left, right| {
        (&right.snapshot.created_at, &right.snapshot.id)
            .cmp(&(&left.snapshot.created_at, &left.snapshot.id))
    });
    Ok(records)
}

fn rollback_snapshot_directory(
    instance_mods: &ManagedStorageDirectory,
    snapshot_id: &str,
) -> Result<ManagedStorageDirectory, StateError> {
    validate_rollback_snapshot_id(snapshot_id)?;
    let history = instance_mods.open_relative_directory(Path::new(&format!(
        "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}/{ROLLBACK_HISTORY_DIR_NAME}"
    )))?;
    history.open_child(snapshot_id)?.ok_or_else(|| {
        StateError::InvalidRollback("rollback snapshot directory is missing".to_string())
    })
}

fn read_snapshot_directory(
    directory: &ManagedStorageDirectory,
    expected_id: &str,
) -> Result<RollbackSnapshot, StateError> {
    let snapshot = read_rollback_candidate(directory, expected_id)?;
    if complete_entries(directory, "rollback snapshot")?.len() != snapshot.artifacts.len() + 1 {
        return Err(StateError::InvalidRollback(
            "rollback snapshot is incomplete".to_string(),
        ));
    }
    Ok(snapshot)
}

fn read_rollback_candidate(
    directory: &ManagedStorageDirectory,
    expected_id: &str,
) -> Result<RollbackSnapshot, StateError> {
    let (snapshot, _) = read_rollback_metadata(directory, expected_id)?;
    let expected_entries = snapshot
        .artifacts
        .iter()
        .map(|artifact| artifact.stored_filename.as_str())
        .chain(std::iter::once(ROLLBACK_METADATA_FILE_NAME))
        .collect::<HashSet<_>>();
    let entries = complete_entries(directory, "rollback snapshot")?;
    let mut present = HashSet::new();
    for entry in entries {
        let name = entry_name(&entry, "rollback snapshot entry")?;
        if entry.kind() != EntryKind::File
            || !expected_entries.contains(name.as_str())
            || !present.insert(name)
        {
            return Err(StateError::InvalidRollback(
                "rollback snapshot contains an unexpected entry".to_string(),
            ));
        }
    }
    if !present.contains(ROLLBACK_METADATA_FILE_NAME) {
        return Err(StateError::InvalidRollback(
            "rollback candidate has no durable intent".to_string(),
        ));
    }
    for artifact in &snapshot.artifacts {
        if !present.contains(&artifact.stored_filename) {
            continue;
        }
        let file = directory.open_file(Path::new(&artifact.stored_filename))?;
        if file.size() != artifact.size
            || !file
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&artifact.sha512)
        {
            return Err(StateError::InvalidRollback(format!(
                "rollback artifact {} failed exact integrity validation",
                artifact.filename
            )));
        }
    }
    Ok(snapshot)
}

fn read_rollback_metadata(
    directory: &ManagedStorageDirectory,
    expected_id: &str,
) -> Result<(RollbackSnapshot, u64), StateError> {
    let metadata = read_bounded_file(
        directory,
        Path::new(ROLLBACK_METADATA_FILE_NAME),
        ROLLBACK_METADATA_MAX_BYTES,
    )?;
    let snapshot = serde_json::from_slice::<RollbackSnapshot>(&metadata.bytes)?;
    validate_rollback_snapshot(&snapshot)?;
    if snapshot.id != expected_id {
        return Err(StateError::InvalidRollback(
            "rollback snapshot id does not match its directory".to_string(),
        ));
    }
    Ok((snapshot, metadata.file.size()))
}

fn complete_rollback_candidate(
    instance_mods: &ManagedStorageDirectory,
    candidate: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<(), RollbackCandidateCompletionError> {
    let entries = complete_entries(candidate, "rollback candidate")?;
    let present = entries
        .iter()
        .filter_map(DirectoryEntry::utf8_name)
        .collect::<HashSet<_>>();
    for artifact in &snapshot.artifacts {
        if present.contains(artifact.stored_filename.as_str()) {
            continue;
        }
        let source = match instance_mods.open_file(Path::new(&artifact.filename)) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RollbackCandidateCompletionError::Unresumable(
                    StateError::RollbackCandidateUnresumable,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if source.size() != artifact.size
            || !source
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&artifact.sha512)
        {
            return Err(RollbackCandidateCompletionError::Unresumable(
                StateError::RollbackCandidateUnresumable,
            ));
        }
        let copied = candidate.copy_file_create_new(
            &source,
            Path::new(&artifact.stored_filename),
            MANAGED_ARTIFACT_MAX_BYTES,
        )?;
        if copied.size() != artifact.size
            || !copied
                .sha512(MANAGED_ARTIFACT_MAX_BYTES)?
                .eq_ignore_ascii_case(&artifact.sha512)
        {
            return Err(RollbackCandidateCompletionError::Other(
                StateError::InvalidRollback(format!(
                    "rollback copy {} failed exact integrity validation",
                    artifact.filename
                )),
            ));
        }
    }
    candidate.sync()?;
    read_snapshot_directory(candidate, &snapshot.id)
        .map(|_| ())
        .map_err(Into::into)
}

fn discard_unresumable_rollback_candidate(
    instance_mods: &ManagedStorageDirectory,
    tmp: &ManagedStorageDirectory,
    candidate_name: &str,
    candidate: ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    if read_rollback_candidate(&candidate, &snapshot.id)? != *snapshot {
        return Err(StateError::InvalidRollback(
            "rollback candidate changed before deletion".to_string(),
        ));
    }
    let delete_name = format!("{ROLLBACK_CANDIDATE_DELETE_PREFIX}{}", snapshot.id);
    if tmp.open_child(&delete_name)?.is_some() {
        return Err(StateError::InvalidRollback(
            "rollback candidate deletion receipt already exists".to_string(),
        ));
    }
    let receipt = instance_mods.move_child_directory_no_replace(candidate, tmp, &delete_name)?;
    tmp.sync()?;
    delete_rollback_directory_receipt(
        instance_mods,
        tmp,
        &delete_name,
        &receipt,
        snapshot,
        &PathBuf::from(STATE_DIR_NAME)
            .join(ROLLBACK_DIR_NAME)
            .join(ROLLBACK_TMP_DIR_NAME),
    )?;
    if tmp.open_child(candidate_name)?.is_some() {
        return Err(StateError::InvalidRollback(
            "rollback candidate remained after deletion transition".to_string(),
        ));
    }
    Ok(())
}

fn reconcile_deleted_rollback_candidates(
    instance_mods: &ManagedStorageDirectory,
    tmp: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    for entry in complete_entries(tmp, "rollback candidate cleanup")? {
        let Some(name) = entry.utf8_name() else {
            return Err(StateError::InvalidRollback(
                "rollback candidate cleanup name is not UTF-8".to_string(),
            ));
        };
        let Some(original) = name.strip_prefix(ROLLBACK_CANDIDATE_DELETE_PREFIX) else {
            continue;
        };
        validate_rollback_snapshot_id(original)?;
        if entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidRollback(
                "rollback candidate deletion receipt is not a directory".to_string(),
            ));
        }
        let receipt = tmp.open_observed_child(&entry)?;
        if complete_entries(&receipt, "rollback candidate deletion receipt")?.is_empty() {
            remove_empty_child(tmp, name)?;
            continue;
        }
        let snapshot = read_rollback_candidate(&receipt, original)?;
        delete_rollback_directory_receipt(
            instance_mods,
            tmp,
            name,
            &receipt,
            &snapshot,
            &PathBuf::from(STATE_DIR_NAME)
                .join(ROLLBACK_DIR_NAME)
                .join(ROLLBACK_TMP_DIR_NAME),
        )?;
    }
    Ok(())
}

fn prune_rollback_history(instance_mods: &ManagedStorageDirectory) -> Result<(), StateError> {
    let records = load_retained_rollback_snapshots(instance_mods)?;
    let mut retained_bytes = 0_u64;
    for (index, record) in records.into_iter().enumerate() {
        let keep = index < ROLLBACK_HISTORY_LIMIT
            && retained_bytes
                .checked_add(record.storage_bytes)
                .is_some_and(|total| total <= ROLLBACK_RETAINED_MAX_BYTES);
        if keep {
            retained_bytes += record.storage_bytes;
        } else {
            delete_snapshot_directory(instance_mods, &record.snapshot)?;
        }
    }
    Ok(())
}

fn retained_rollback_storage_bytes(
    instance_mods: &ManagedStorageDirectory,
) -> Result<u64, StateError> {
    load_retained_rollback_snapshots(instance_mods)?
        .into_iter()
        .try_fold(0_u64, |total, record| {
            total.checked_add(record.storage_bytes).ok_or_else(|| {
                StateError::InvalidRollback(
                    "rollback retained byte budget overflowed".to_string(),
                )
            })
        })
}

fn delete_snapshot_directory(
    instance_mods: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    let history = instance_mods.open_relative_directory(Path::new(&format!(
        "{STATE_DIR_NAME}/{ROLLBACK_DIR_NAME}/{ROLLBACK_HISTORY_DIR_NAME}"
    )))?;
    let directory = rollback_snapshot_directory(instance_mods, &snapshot.id)?;
    if read_snapshot_directory(&directory, &snapshot.id)? != *snapshot {
        return Err(StateError::InvalidRollback(
            "rollback pruning source changed before deletion".to_string(),
        ));
    }
    let delete_name = format!("{ROLLBACK_DELETE_PREFIX}{}", snapshot.id);
    if history.open_child(&delete_name)?.is_some() {
        return Err(StateError::InvalidRollback(
            "rollback deletion receipt already exists".to_string(),
        ));
    }
    let receipt = instance_mods.move_child_directory_no_replace(
        directory,
        &history,
        &delete_name,
    )?;
    history.sync()?;
    delete_snapshot_receipt(instance_mods, &history, &delete_name, &receipt, snapshot)
}

fn delete_snapshot_receipt(
    instance_mods: &ManagedStorageDirectory,
    history: &ManagedStorageDirectory,
    delete_name: &str,
    receipt: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<(), StateError> {
    delete_rollback_directory_receipt(
        instance_mods,
        history,
        delete_name,
        receipt,
        snapshot,
        &PathBuf::from(STATE_DIR_NAME)
            .join(ROLLBACK_DIR_NAME)
            .join(ROLLBACK_HISTORY_DIR_NAME),
    )
}

fn delete_rollback_directory_receipt(
    instance_mods: &ManagedStorageDirectory,
    parent: &ManagedStorageDirectory,
    receipt_name: &str,
    receipt: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
    receipt_parent_relative: &Path,
) -> Result<(), StateError> {
    let admitted = read_rollback_candidate(receipt, &snapshot.id)?;
    if admitted != *snapshot {
        return Err(StateError::InvalidRollback(
            "rollback deletion receipt metadata changed".to_string(),
        ));
    }
    for artifact in &snapshot.artifacts {
        if receipt
            .open_file_if_present(Path::new(&artifact.stored_filename))?
            .is_none()
        {
            continue;
        }
        let relative = receipt_parent_relative
            .join(receipt_name)
            .join(&artifact.stored_filename);
        quarantine_remove_exact(instance_mods, &relative, &artifact.sha512, artifact.size)?;
    }
    let metadata = read_bounded_file(
        receipt,
        Path::new(ROLLBACK_METADATA_FILE_NAME),
        ROLLBACK_METADATA_MAX_BYTES,
    )?;
    let relative = receipt_parent_relative
        .join(receipt_name)
        .join(ROLLBACK_METADATA_FILE_NAME);
    quarantine_remove_exact(
        instance_mods,
        &relative,
        &metadata.sha512,
        metadata.file.size(),
    )?;
    remove_empty_child(parent, receipt_name)
}

fn reconcile_deleted_snapshot_directories(
    instance_mods: &ManagedStorageDirectory,
    history: &ManagedStorageDirectory,
) -> Result<(), StateError> {
    reconcile_empty_directory_parks(history, "rollback history cleanup")?;
    for entry in complete_entries(history, "rollback history")? {
        let Some(name) = entry.utf8_name() else {
            return Err(StateError::InvalidRollback(
                "rollback history name is not UTF-8".to_string(),
            ));
        };
        let Some(original) = name.strip_prefix(ROLLBACK_DELETE_PREFIX) else {
            continue;
        };
        validate_rollback_snapshot_id(original)?;
        if entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidRollback(
                "rollback deletion receipt is not a directory".to_string(),
            ));
        }
        let receipt = history.open_observed_child(&entry)?;
        if complete_entries(&receipt, "rollback deletion receipt")?.is_empty() {
            remove_empty_child(history, name)?;
            continue;
        }
        let snapshot = read_rollback_candidate(&receipt, original)?;
        delete_snapshot_receipt(instance_mods, history, name, &receipt, &snapshot)?;
    }
    Ok(())
}

fn reconcile_empty_directory_parks(
    parent: &ManagedStorageDirectory,
    label: &str,
) -> Result<(), StateError> {
    for entry in complete_entries(parent, label)? {
        let Some(name) = entry.utf8_name() else {
            return Err(StateError::InvalidState(format!("{label} name is not UTF-8")));
        };
        let Some(original) = name.strip_prefix(EMPTY_DIRECTORY_PARK_PREFIX) else {
            continue;
        };
        if original.is_empty() || entry.kind() != EntryKind::Directory {
            return Err(StateError::InvalidState(format!(
                "{label} park receipt is invalid"
            )));
        }
        if parent.open_child(original)?.is_some() {
            return Err(StateError::InvalidState(format!(
                "{label} has both live and parked bindings"
            )));
        }
        let parked_directory = parent.open_observed_child(&entry)?;
        if !complete_entries(&parked_directory, label)?.is_empty() {
            return Err(StateError::InvalidState(format!(
                "{label} parked directory is not empty"
            )));
        }
        let parked = parent.admit_existing_directory_park(original, name)?;
        settle_parked_directory_removal(parent, parked)?;
    }
    Ok(())
}

fn remove_empty_child(
    parent: &ManagedStorageDirectory,
    name: &str,
) -> Result<(), StateError> {
    let park_name = format!("{EMPTY_DIRECTORY_PARK_PREFIX}{name}");
    if parent.open_child(&park_name)?.is_some() {
        if parent.open_child(name)?.is_some() {
            return Err(StateError::InvalidState(
                "directory removal has both live and parked bindings".to_string(),
            ));
        }
        let parked = parent.admit_existing_directory_park(name, &park_name)?;
        return settle_parked_directory_removal(parent, parked).map_err(StateError::Read);
    }
    let Some(child) = parent.open_child(name)? else {
        return Ok(());
    };
    if !complete_entries(&child, "empty managed directory")?.is_empty() {
        return Err(StateError::InvalidState(
            "managed directory is not empty during removal".to_string(),
        ));
    }
    let parked = parent.park_child_directory_as(child, &park_name)?;
    settle_parked_directory_removal(parent, parked).map_err(StateError::Read)
}

fn read_bounded_file_if_present(
    root: &ManagedStorageDirectory,
    relative: &Path,
    max_bytes: u64,
) -> Result<Option<AdmittedFile>, StateError> {
    let Some(file) = root.open_file_if_present(relative)? else {
        return Ok(None);
    };
    if file.size() > max_bytes {
        return Err(StateError::InvalidState(
            "managed file exceeds its byte budget".to_string(),
        ));
    }
    let bytes = file.read_bounded(max_bytes)?;
    let sha256 = Sha256::digest(&bytes).into();
    let sha512 = hex::encode(Sha512::digest(&bytes));
    Ok(Some(AdmittedFile {
        file,
        bytes,
        sha256,
        sha512,
    }))
}

fn read_bounded_file(
    root: &ManagedStorageDirectory,
    relative: &Path,
    max_bytes: u64,
) -> Result<AdmittedFile, StateError> {
    read_bounded_file_if_present(root, relative, max_bytes)?.ok_or_else(|| {
        StateError::InvalidState("managed file is missing".to_string())
    })
}

fn open_optional_directory(
    root: &ManagedStorageDirectory,
    relative: &Path,
) -> Result<Option<ManagedStorageDirectory>, StateError> {
    match root.open_relative_directory(relative) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StateError::Read(error)),
    }
}

fn directory_has_entries(
    root: &ManagedStorageDirectory,
    relative: &Path,
    label: &str,
) -> Result<bool, StateError> {
    let Some(directory) = open_optional_directory(root, relative)? else {
        return Ok(false);
    };
    Ok(!complete_entries(&directory, label)?.is_empty())
}

fn complete_entries(
    directory: &ManagedStorageDirectory,
    label: &str,
) -> Result<Vec<DirectoryEntry>, StateError> {
    let listing = directory.entries(RECOVERY_ENTRY_LIMIT + 1)?;
    if listing.state() != DirectoryListingState::Complete
        || listing.entries().len() > RECOVERY_ENTRY_LIMIT
    {
        return Err(StateError::InvalidState(format!(
            "{label} exceed the recovery entry limit"
        )));
    }
    Ok(listing.entries().to_vec())
}

fn entry_name(entry: &DirectoryEntry, label: &str) -> Result<String, StateError> {
    entry
        .utf8_name()
        .map(str::to_string)
        .ok_or_else(|| StateError::InvalidState(format!("{label} is not UTF-8")))
}

fn addition_marker_name(installed: &InstalledMod) -> String {
    let mut digest = Sha256::new();
    digest.update(installed.filename.as_bytes());
    digest.update([0]);
    digest.update(installed.integrity.sha512.as_bytes());
    format!("{}.json", hex::encode(digest.finalize()))
}

fn addition_marker_relative(marker_name: &str) -> PathBuf {
    PathBuf::from(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(ADDITION_DIR_NAME)
        .join(marker_name)
}

fn validate_addition_marker(name: &str, marker: &AdditionMarker) -> Result<(), StateError> {
    if marker.schema_version != ADDITION_MARKER_SCHEMA_VERSION
        || name != addition_marker_name(&marker.artifact)
    {
        return Err(StateError::InvalidState(
            "managed addition marker identity is invalid".to_string(),
        ));
    }
    validate_managed_filename(&marker.artifact.filename)?;
    validate_state_token("managed project id", &marker.artifact.project_id)?;
    validate_state_token("managed version id", &marker.artifact.version_id)?;
    if marker.artifact.ownership_class != OwnershipClass::CompositionManaged {
        return Err(StateError::InvalidOwnership {
            filename: marker.artifact.filename.clone(),
            ownership_class: format!("{:?}", marker.artifact.ownership_class),
        });
    }
    if marker.artifact.size == 0 || marker.artifact.size > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(StateError::InvalidIntegrity {
            filename: marker.artifact.filename.clone(),
            reason: "managed addition exceeds the byte budget".to_string(),
        });
    }
    validate_sha512_integrity(
        &marker.artifact.filename,
        &marker.artifact.integrity.sha512,
    )
}

fn removal_backup_relative(installed: &InstalledMod) -> PathBuf {
    removal_backup_relative_parts(&installed.integrity.sha512, &installed.filename)
}

fn removal_backup_relative_parts(digest: &str, filename: &str) -> PathBuf {
    PathBuf::from(STATE_DIR_NAME)
        .join(MUTATION_DIR_NAME)
        .join(REMOVAL_DIR_NAME)
        .join(digest.to_ascii_lowercase())
        .join(filename)
}

fn quarantine_leaf(relative: &Path) -> Result<String, StateError> {
    let logical = relative.to_str().ok_or_else(|| {
        StateError::InvalidState("managed cleanup name is not UTF-8".to_string())
    })?;
    Ok(format!(
        "{}.park",
        hex::encode(Sha256::digest(logical.as_bytes()))
    ))
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
        let filename_key = portable_filename_key(&installed.filename)?;
        validate_state_token("managed project id", &installed.project_id)?;
        validate_state_token("managed version id", &installed.version_id)?;
        if !project_ids.insert(installed.project_id.to_ascii_lowercase()) {
            return Err(StateError::InvalidState(
                "performance state contains colliding project ids".to_string(),
            ));
        }
        if !filenames.insert(filename_key) {
            return Err(StateError::InvalidState(
                "performance state contains colliding filenames".to_string(),
            ));
        }
        if installed.ownership_class != OwnershipClass::CompositionManaged {
            return Err(StateError::InvalidOwnership {
                filename: installed.filename.clone(),
                ownership_class: format!("{:?}", installed.ownership_class),
            });
        }
        if installed.size > MANAGED_ARTIFACT_MAX_BYTES {
            return Err(StateError::InvalidIntegrity {
                filename: installed.filename.clone(),
                reason: "managed artifact exceeds the byte budget".to_string(),
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

fn validate_sha512_integrity(filename: &str, sha512: &str) -> Result<(), StateError> {
    if !is_valid_sha512(sha512) {
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

fn validate_managed_filename(filename: &str) -> Result<(), StateError> {
    portable_filename_key(filename).map(|_| ())
}

fn portable_filename_key(filename: &str) -> Result<PortablePathKey, StateError> {
    PortableFileName::new_exact(filename)
        .map(|filename| filename.key())
        .map_err(|_| StateError::InvalidFilename(filename.to_string()))
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
    if snapshot.artifacts.len() > STATE_MAX_INSTALLED_MODS {
        return Err(StateError::InvalidRollback(
            "rollback artifact count exceeds the limit".to_string(),
        ));
    }
    validate_rollback_artifact_budget(snapshot)?;
    if let Some(state) = snapshot.state() {
        validate_state(state)?;
    }
    let mut artifact_filenames = HashSet::new();
    let mut stored_filenames = HashSet::new();
    for artifact in &snapshot.artifacts {
        if !artifact_filenames.insert(portable_filename_key(&artifact.filename)?)
            || !stored_filenames.insert(portable_filename_key(&artifact.stored_filename)?)
        {
            return Err(StateError::InvalidRollback(
                "rollback contains colliding artifact identities".to_string(),
            ));
        }
        validate_state_token("rollback project id", &artifact.project_id)?;
        validate_state_token("rollback version id", &artifact.version_id)?;
        validate_sha512_integrity(&artifact.filename, &artifact.sha512)?;
        let installed = snapshot
            .state()
            .into_iter()
            .flat_map(|state| state.installed_mods.iter())
            .find(|installed| installed.filename == artifact.filename)
            .ok_or_else(|| {
                StateError::InvalidRollback(format!(
                    "artifact {} is not in the rollback state",
                    artifact.filename
                ))
            })?;
        if artifact.project_id != installed.project_id
            || artifact.version_id != installed.version_id
            || artifact.ownership_class != OwnershipClass::CompositionManaged
            || artifact.ownership_class != installed.ownership_class
            || artifact.size != installed.size
            || !artifact.sha512.eq_ignore_ascii_case(&installed.integrity.sha512)
        {
            return Err(StateError::InvalidRollback(format!(
                "artifact {} metadata does not match rollback state",
                artifact.filename
            )));
        }
    }
    if artifact_filenames.len() != snapshot.state().map_or(0, |state| state.installed_mods.len()) {
        return Err(StateError::InvalidRollback(
            "rollback artifacts do not cover the managed state".to_string(),
        ));
    }
    Ok(())
}

fn validate_rollback_artifact_budget(snapshot: &RollbackSnapshot) -> Result<(), StateError> {
    let total = snapshot.artifacts.iter().try_fold(0_u64, |total, artifact| {
        total.checked_add(artifact.size).ok_or_else(|| {
            StateError::InvalidRollback("rollback artifact byte total overflowed".to_string())
        })
    })?;
    if total > MANAGED_ARTIFACT_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback snapshot exceeds the aggregate artifact budget".to_string(),
        ));
    }
    Ok(())
}

fn rollback_snapshot_storage_bytes(
    snapshot: &RollbackSnapshot,
    metadata_bytes: u64,
) -> Result<u64, StateError> {
    if metadata_bytes > ROLLBACK_METADATA_MAX_BYTES {
        return Err(StateError::InvalidRollback(
            "rollback metadata exceeds the byte budget".to_string(),
        ));
    }
    snapshot
        .artifacts
        .iter()
        .try_fold(metadata_bytes, |total, artifact| {
            total.checked_add(artifact.size).ok_or_else(|| {
                StateError::InvalidRollback("rollback storage byte budget overflowed".to_string())
            })
        })
}

fn rollback_directory_storage_bytes(
    directory: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
) -> Result<u64, StateError> {
    let metadata = directory.open_file(Path::new(ROLLBACK_METADATA_FILE_NAME))?;
    rollback_snapshot_storage_bytes(snapshot, metadata.size())
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

fn same_artifact_identity(left: &InstalledMod, right: &InstalledMod) -> bool {
    left.filename == right.filename
        && left.size == right.size
        && left.integrity.sha512.eq_ignore_ascii_case(&right.integrity.sha512)
}

fn new_rollback_snapshot_id() -> String {
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce);
    format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        hex::encode(nonce)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::TestManagedStorage;
    use crate::types::{
        ManagedArtifactIntegrity, ManagedArtifactProvider, ManagedArtifactRole,
        ManagedArtifactSource, ManagedDependencyStateEdge, VersionFamily,
    };
    use std::fs;

    #[test]
    fn capability_state_round_trip_and_removal() {
        let root = test_root("state-round-trip");
        fs::create_dir_all(&root).expect("create root");
        let storage = TestManagedStorage::new(&root);
        let state = test_state(Vec::new());
        save_state(storage.directory(), &state).expect("save state");
        assert_eq!(load_state(storage.directory()).expect("load state"), Some(state));
        remove_state(storage.directory()).expect("remove state");
        assert_eq!(load_state(storage.directory()).expect("load empty state"), None);
    }

    #[test]
    fn rollback_snapshot_restores_managed_bytes_and_state() {
        let root = test_root("rollback-restore");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("managed.jar"), b"managed-v1").expect("write managed artifact");
        let storage = TestManagedStorage::new(&root);
        let installed = test_installed("managed.jar", b"managed-v1");
        let state = test_state(vec![installed.clone()]);
        save_state(storage.directory(), &state).expect("save state");
        let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");

        stage_managed_artifact_removal(storage.directory(), &installed).expect("stage removal");
        remove_state(storage.directory()).expect("remove state");
        settle_managed_artifact_removal(storage.directory(), &installed).expect("settle removal");
        restore_rollback_snapshot(storage.directory(), &snapshot).expect("restore snapshot");

        assert_eq!(fs::read(root.join("managed.jar")).expect("read restored"), b"managed-v1");
        assert_eq!(load_state(storage.directory()).expect("load restored"), Some(state));
    }

    #[test]
    fn rollback_failure_before_state_publication_preserves_old_authority() {
        let fixture = restore_compensation_fixture("rollback-before-state-publication");

        let result = restore_with_fault(
            fixture.storage.directory(),
            &fixture.snapshot,
            RollbackRestoreFaultPoint::BeforeStatePublication,
        );

        assert!(matches!(
            result,
            Err(RollbackRestoreError::Indeterminate(StateError::InvalidState(message)))
                if message == "injected rollback restore failure"
        ));
        assert_compensated_restore(
            &fixture,
            &fixture.current_state,
            "current.jar",
            b"current-managed",
            "target.jar",
        );
    }

    #[test]
    fn rollback_failure_after_state_publication_preserves_target_authority() {
        let fixture = restore_compensation_fixture("rollback-after-state-publication");

        let result = restore_with_fault(
            fixture.storage.directory(),
            &fixture.snapshot,
            RollbackRestoreFaultPoint::AfterStatePublication,
        );

        assert!(matches!(
            result,
            Err(RollbackRestoreError::Indeterminate(StateError::InvalidState(message)))
                if message == "injected rollback restore failure"
        ));
        assert_compensated_restore(
            &fixture,
            &fixture.target_state,
            "target.jar",
            b"target-managed",
            "current.jar",
        );
    }

    #[test]
    fn rollback_recovery_completes_a_metadata_first_partial_candidate() {
        let root = test_root("rollback-partial-candidate");
        let (storage, snapshot, candidate) = prepare_partial_candidate(&root);

        reconcile_rollback_metadata(storage.directory()).expect("resume candidate");

        assert!(!candidate.exists());
        assert_eq!(
            load_rollback_snapshot_by_id_admitted(storage.directory(), &snapshot.id)
                .expect("load recovered snapshot"),
            Some(snapshot)
        );
    }

    #[test]
    fn unresumable_partial_candidate_is_durably_discarded() {
        let root = test_root("rollback-unresumable-candidate");
        let (storage, snapshot, candidate) = prepare_partial_candidate(&root);
        let tmp = rollback_tmp_path(&root);
        fs::remove_file(root.join("second.jar")).expect("remove recovery source");

        assert!(matches!(
            reconcile_rollback_metadata(storage.directory()),
            Err(StateError::RollbackCandidateUnresumable)
        ));
        assert!(!candidate.exists());
        assert!(
            !tmp.join(format!(
                "{ROLLBACK_CANDIDATE_DELETE_PREFIX}{}",
                snapshot.id
            ))
            .exists()
        );
        reconcile_rollback_metadata(storage.directory())
            .expect("discarded candidate must not wedge recovery");
    }

    #[test]
    fn changed_source_discards_an_unresumable_partial_candidate() {
        let root = test_root("rollback-changed-source");
        let (storage, snapshot, candidate) = prepare_partial_candidate(&root);
        let tmp = rollback_tmp_path(&root);
        fs::write(root.join("second.jar"), b"changed-managed-second")
            .expect("replace recovery source");

        assert!(matches!(
            reconcile_rollback_metadata(storage.directory()),
            Err(StateError::RollbackCandidateUnresumable)
        ));
        assert!(!candidate.exists());
        assert!(
            !tmp.join(format!(
                "{ROLLBACK_CANDIDATE_DELETE_PREFIX}{}",
                snapshot.id
            ))
            .exists()
        );
        assert_eq!(
            fs::read(root.join("second.jar")).expect("read changed source"),
            b"changed-managed-second"
        );
        reconcile_rollback_metadata(storage.directory())
            .expect("discarded changed-source candidate must not wedge recovery");
    }

    #[test]
    fn unknown_candidate_entry_fails_closed_and_is_preserved() {
        let root = test_root("rollback-unknown-candidate-entry");
        let (storage, _, candidate) = prepare_partial_candidate(&root);
        let unknown = candidate.join("unknown.bin");
        fs::write(&unknown, b"unowned").expect("write unknown candidate entry");

        assert!(matches!(
            reconcile_rollback_metadata(storage.directory()),
            Err(StateError::InvalidRollback(reason))
                if reason == "rollback snapshot contains an unexpected entry"
        ));
        assert!(candidate.exists());
        assert_eq!(fs::read(unknown).expect("read preserved unknown entry"), b"unowned");
    }

    #[test]
    fn rollback_deletion_receipt_resumes_with_metadata_last() {
        let root = test_root("rollback-delete-receipt");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("first.jar"), b"managed-first").expect("write first artifact");
        fs::write(root.join("second.jar"), b"managed-second").expect("write second artifact");
        let storage = TestManagedStorage::new(&root);
        let state = test_state(vec![
            test_installed_identity("first.jar", b"managed-first", "AANobbMI", "NFkjnzWE"),
            test_installed_identity("second.jar", b"managed-second", "BBNobbMI", "OFkjnzWE"),
        ]);
        let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");
        let history = rollback_history_path(&root);
        let delete_name = format!("{ROLLBACK_DELETE_PREFIX}{}", snapshot.id);
        let receipt = history.join(&delete_name);
        fs::rename(history.join(&snapshot.id), &receipt).expect("publish deletion receipt");
        fs::remove_file(receipt.join(&snapshot.artifacts[0].stored_filename))
            .expect("interrupt artifact deletion");

        reconcile_rollback_metadata(storage.directory()).expect("resume deletion receipt");

        assert!(!receipt.exists());
        assert!(
            list_rollback_snapshots_admitted(storage.directory())
                .expect("list history")
                .is_empty()
        );
    }

    #[test]
    fn rollback_storage_budget_counts_actual_metadata_bytes() {
        let root = test_root("rollback-metadata-budget");
        fs::create_dir_all(&root).expect("create root");
        let storage = TestManagedStorage::new(&root);
        let snapshot = save_absent_rollback_snapshot(storage.directory()).expect("snapshot");
        let metadata_path = rollback_history_path(&root)
            .join(&snapshot.id)
            .join(ROLLBACK_METADATA_FILE_NAME);
        let mut metadata = fs::read(&metadata_path).expect("read metadata");
        metadata.extend(std::iter::repeat_n(b' ', 4096));
        fs::write(&metadata_path, &metadata).expect("expand metadata whitespace");
        let directory = rollback_snapshot_directory(storage.directory(), &snapshot.id)
            .expect("open snapshot directory");
        let admitted = read_snapshot_directory(&directory, &snapshot.id).expect("read snapshot");

        assert_eq!(
            rollback_directory_storage_bytes(&directory, &admitted)
                .expect("measure snapshot storage"),
            metadata.len() as u64
        );
    }

    #[test]
    fn persisted_candidate_rejects_aggregate_budget_before_source_copy() {
        let root = test_root("rollback-persisted-aggregate-budget");
        let candidate_id = "aggregate-budget";
        let candidate = rollback_tmp_path(&root)
            .join(format!("{ROLLBACK_CANDIDATE_PREFIX}{candidate_id}"));
        fs::create_dir_all(&candidate).expect("create rollback candidate");
        let snapshot = RollbackSnapshot {
            id: candidate_id.to_string(),
            schema_version: ROLLBACK_SCHEMA_VERSION,
            created_at: Utc::now().to_rfc3339(),
            target: RollbackSnapshotState::ManagedStateAbsent,
            artifacts: vec![
                test_rollback_artifact("first.jar", MANAGED_ARTIFACT_MAX_BYTES),
                test_rollback_artifact("second.jar", 1),
            ],
        };
        fs::write(
            candidate.join(ROLLBACK_METADATA_FILE_NAME),
            serde_json::to_vec_pretty(&snapshot).expect("serialize oversized snapshot"),
        )
        .expect("write oversized snapshot intent");
        let storage = TestManagedStorage::new(&root);

        assert!(matches!(
            reconcile_rollback_metadata(storage.directory()),
            Err(StateError::InvalidRollback(reason))
                if reason == "rollback snapshot exceeds the aggregate artifact budget"
        ));
        assert_eq!(
            fs::read_dir(&candidate)
                .expect("read rejected candidate")
                .map(|entry| entry.expect("read candidate entry").file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from(ROLLBACK_METADATA_FILE_NAME)]
        );
    }

    fn test_installed(filename: &str, bytes: &[u8]) -> InstalledMod {
        test_installed_identity(filename, bytes, "AANobbMI", "NFkjnzWE")
    }

    fn test_installed_identity(
        filename: &str,
        bytes: &[u8],
        project_id: &str,
        version_id: &str,
    ) -> InstalledMod {
        InstalledMod {
            project_id: project_id.to_string(),
            version_id: version_id.to_string(),
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

    fn test_rollback_artifact(filename: &str, size: u64) -> RollbackArtifact {
        RollbackArtifact {
            filename: filename.to_string(),
            stored_filename: format!("{filename}.bin"),
            project_id: "AANobbMI".to_string(),
            version_id: "NFkjnzWE".to_string(),
            ownership_class: OwnershipClass::CompositionManaged,
            size,
            sha512: "0".repeat(128),
        }
    }

    struct RestoreCompensationFixture {
        root: PathBuf,
        storage: TestManagedStorage,
        snapshot: RollbackSnapshot,
        target_state: CompositionState,
        current_state: CompositionState,
    }

    fn restore_compensation_fixture(name: &str) -> RestoreCompensationFixture {
        let root = test_root(name);
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("target.jar"), b"target-managed")
            .expect("write target artifact");
        let storage = TestManagedStorage::new(&root);
        let target = test_installed_identity(
            "target.jar",
            b"target-managed",
            "AANobbMI",
            "NFkjnzWE",
        );
        let target_state = test_state(vec![target.clone()]);
        save_state(storage.directory(), &target_state).expect("save target state");
        let snapshot =
            save_rollback_snapshot(storage.directory(), &target_state).expect("save target snapshot");

        let current = test_installed_identity(
            "current.jar",
            b"current-managed",
            "BBNobbMI",
            "OFkjnzWE",
        );
        let current_state = test_state(vec![current.clone()]);
        stage_managed_artifact_removal(storage.directory(), &target)
            .expect("stage target removal");
        let addition = prepare_managed_artifact_addition(storage.directory(), &current)
            .expect("prepare current addition");
        storage
            .directory()
            .create_file_create_new(Path::new(&current.filename), b"current-managed")
            .expect("create current artifact");
        publish_managed_artifact_addition(storage.directory(), &current, &addition)
            .expect("publish current addition");
        save_state(storage.directory(), &current_state).expect("save current state");
        reconcile_managed_addition_obligations(storage.directory(), Some(&current_state))
            .expect("settle current addition");
        settle_managed_artifact_removal(storage.directory(), &target)
            .expect("settle target removal");

        RestoreCompensationFixture {
            root,
            storage,
            snapshot,
            target_state,
            current_state,
        }
    }

    fn restore_with_fault(
        instance_mods: &ManagedStorageDirectory,
        snapshot: &RollbackSnapshot,
        point: RollbackRestoreFaultPoint,
    ) -> Result<ManagedRollbackOutcome, RollbackRestoreError> {
        ROLLBACK_RESTORE_FAULT.with(|fault| {
            assert_eq!(fault.replace(Some(point)), None, "restore fault already armed");
        });
        let result = restore_rollback_snapshot_classified(instance_mods, snapshot);
        ROLLBACK_RESTORE_FAULT.with(|fault| fault.set(None));
        result
    }

    fn assert_compensated_restore(
        fixture: &RestoreCompensationFixture,
        expected_state: &CompositionState,
        expected_filename: &str,
        expected_bytes: &[u8],
        absent_filename: &str,
    ) {
        let instance_mods = fixture.storage.directory();
        let admitted = load_state_admitted(instance_mods).expect("load compensated state");
        assert_eq!(admitted.as_ref(), Some(expected_state));
        assert_eq!(
            fs::read(fixture.root.join(expected_filename)).expect("read authoritative artifact"),
            expected_bytes
        );
        assert!(!fixture.root.join(absent_filename).exists());
        assert!(!managed_effect_reconciliation_required(instance_mods));
        assert_eq!(
            preflight_managed_inspection_reconciliation(instance_mods)
                .expect("preflight compensated storage"),
            ManagedInspectionReconciliation::default()
        );
        prove_managed_storage_recovered(instance_mods, admitted.as_ref())
            .expect("prove compensated storage");
        assert_no_pending_park_receipts(&fixture.root);
    }

    fn assert_no_pending_park_receipts(root: &Path) {
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).expect("read managed test directory") {
                let entry = entry.expect("read managed test entry");
                let name = entry.file_name();
                let name = name.to_string_lossy();
                assert!(
                    !name.starts_with(EMPTY_DIRECTORY_PARK_PREFIX)
                        && !name.starts_with(ROLLBACK_DELETE_PREFIX),
                    "pending managed park or deletion receipt remained: {name}"
                );
                if entry.file_type().expect("read managed test entry type").is_dir() {
                    pending.push(entry.path());
                }
            }
        }
    }

    fn prepare_partial_candidate(
        root: &Path,
    ) -> (TestManagedStorage, RollbackSnapshot, PathBuf) {
        fs::create_dir_all(root).expect("create root");
        fs::write(root.join("first.jar"), b"managed-first").expect("write first artifact");
        fs::write(root.join("second.jar"), b"managed-second")
            .expect("write second artifact");
        let storage = TestManagedStorage::new(root);
        let state = test_state(vec![
            test_installed_identity("first.jar", b"managed-first", "AANobbMI", "NFkjnzWE"),
            test_installed_identity("second.jar", b"managed-second", "BBNobbMI", "OFkjnzWE"),
        ]);
        let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");
        let candidate = rollback_tmp_path(root)
            .join(format!("{ROLLBACK_CANDIDATE_PREFIX}{}", snapshot.id));
        fs::rename(rollback_history_path(root).join(&snapshot.id), &candidate)
            .expect("restore candidate namespace");
        let missing = snapshot
            .artifacts
            .iter()
            .find(|artifact| artifact.filename == "second.jar")
            .expect("second snapshot artifact");
        fs::remove_file(candidate.join(&missing.stored_filename))
            .expect("interrupt candidate copy");
        (storage, snapshot, candidate)
    }

    fn test_state(installed_mods: Vec<InstalledMod>) -> CompositionState {
        let mut state = CompositionState {
            composition_id: "test-composition".to_string(),
            family: VersionFamily::A,
            tier: CompositionTier::Core,
            game_version: "1.21.1".to_string(),
            loader: "fabric".to_string(),
            graph_sha512: String::new(),
            dependency_edges: Vec::<ManagedDependencyStateEdge>::new(),
            installed_mods,
            installed_at: Utc::now().to_rfc3339(),
        };
        state.graph_sha512 = crate::install::plan::canonical_state_graph_digest(&state)
            .expect("canonical test state graph");
        state
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-performance-state-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn rollback_history_path(root: &Path) -> PathBuf {
        root.join(STATE_DIR_NAME)
            .join(ROLLBACK_DIR_NAME)
            .join(ROLLBACK_HISTORY_DIR_NAME)
    }

    fn rollback_tmp_path(root: &Path) -> PathBuf {
        root.join(STATE_DIR_NAME)
            .join(ROLLBACK_DIR_NAME)
            .join(ROLLBACK_TMP_DIR_NAME)
    }
}
