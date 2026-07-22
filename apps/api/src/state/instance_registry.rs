use super::instance_lifecycle::InstanceLifecycleIncarnation;
use crate::execution::anchored_record::{AnchoredRecordDirectory, AnchoredRecordObservation};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
#[cfg(test)]
use axial_config::generate_instance_id;
use axial_config::{
    AppPaths, AppRootSession, INSTANCE_REGISTRY_MAX_BYTES, Instance, InstanceRegistrySnapshot,
    InstanceStore, InstanceStoreError, PendingInstanceDeletion, StartupFileProvenance,
    derive_instance_art_seed, is_canonical_instance_id,
};
use axial_fs::{
    Directory, DirectoryListingState, DirectoryParkObligation, DirectoryParkOutcome,
    DirectoryParkResolution, DirectoryRestoreObligation, DirectoryRestoreOutcome,
    DirectoryRestoreResolution, DirectoryTreeRemovalObligation, DirectoryTreeRemovalOutcome,
    DirectoryEntry, DirectoryTreeRemovalResolution, EntryKind, LeafName,
    LeafNameEquivalenceKey, ParkedDirectory, RetainedDirectoryTreeRemoval,
    MAX_DIRECTORY_LIST_ENTRIES, leaf_name_equivalence_keys,
};
use axial_minecraft::managed_path::{
    ManagedTreeDirectory, ManagedTreeOperation, ManagedTreeRetirement, ManagedTreeRoot,
};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock,
    Semaphore,
};

const INSTANCE_REGISTRY_LOCK_INVARIANT: &str =
    "application instance registry lock poisoned; visible state may diverge from persistence";
const INSTANCE_CONTENT_SETTLEMENT_CONCURRENCY: usize = 2;
const INSTANCE_CONTENT_ROOT_LIMIT: usize = 64;
const INSTANCE_TOMBSTONE_NAME_PREFIX: &str = ".axial-instance-tombstone-v1-";

struct InstanceRegistryPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl InstanceRegistryPersistence {
    fn claim(
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, InstanceStoreError> {
        Self::claim_with_coordinator(directory, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, InstanceStoreError> {
        let record = directory
            .target(
                std::ffi::OsStr::new("instances.json"),
                INSTANCE_REGISTRY_MAX_BYTES,
            )
            .map_err(instance_persistence_error)?;
        let owner = coordinator
            .claim_record(record.clone())
            .map_err(instance_persistence_error)?;
        let writer = owner
            .writer(record)
            .map_err(instance_persistence_error)?;
        Ok(Self { owner, writer })
    }
}

struct InstanceRegistryState {
    visible: InstanceRegistrySnapshot,
    retry_candidate: Option<(u64, InstanceRegistrySnapshot)>,
}

struct InstanceDeletionDropGuard {
    armed: bool,
}

impl InstanceDeletionDropGuard {
    fn armed() -> Self {
        Self { armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InstanceDeletionDropGuard {
    fn drop(&mut self) {
        if self.armed {
            std::process::abort();
        }
    }
}

#[must_use = "prepared deletion must be persisted or restored"]
pub(super) struct PreparedInstanceDeletion {
    guard: InstanceDeletionDropGuard,
    store: Arc<AppInstanceStore>,
    gate: OwnedMutexGuard<()>,
    candidate: InstanceRegistrySnapshot,
    encoded: Vec<u8>,
    instance_id: String,
    delete_files: bool,
    files: PreparedInstanceDeletionFiles,
}

#[must_use = "deletion preparation retry must be reconciled"]
pub(super) struct InstanceDeletionPreparationRetry {
    guard: InstanceDeletionDropGuard,
    store: Arc<AppInstanceStore>,
    gate: OwnedMutexGuard<()>,
    candidate: InstanceRegistrySnapshot,
    encoded: Vec<u8>,
    instance_id: String,
    delete_files: bool,
    pending: PendingInstanceDeletion,
    obligation: DirectoryParkObligation,
}

#[must_use = "deletion preparation failure may retain a retry obligation"]
pub(super) enum InstanceDeletionPreparationFailure {
    Refused(InstanceStoreError),
    Retryable {
        error: InstanceStoreError,
        retry: InstanceDeletionPreparationRetry,
    },
}

impl From<InstanceStoreError> for InstanceDeletionPreparationFailure {
    fn from(error: InstanceStoreError) -> Self {
        Self::Refused(error)
    }
}

#[must_use = "accepted deletion persistence must be retried"]
pub(super) struct InstanceDeletionPersistenceRetry {
    guard: InstanceDeletionDropGuard,
    store: Arc<AppInstanceStore>,
    gate: OwnedMutexGuard<()>,
    revision: u64,
    candidate: InstanceRegistrySnapshot,
    instance_id: String,
    delete_files: bool,
    files: PreparedInstanceDeletionFiles,
}

#[must_use = "deletion persistence failure retains transaction ownership"]
pub(super) enum InstanceDeletionPersistenceFailure {
    PreAcceptance {
        error: InstanceStoreError,
        prepared: PreparedInstanceDeletion,
    },
    Retryable {
        error: InstanceStoreError,
        retry: InstanceDeletionPersistenceRetry,
    },
}

#[must_use = "committed deletion files and auxiliaries must be settled"]
pub(super) struct CommittedInstanceDeletion {
    guard: InstanceDeletionDropGuard,
    store: Arc<AppInstanceStore>,
    gate: OwnedMutexGuard<()>,
    instance_id: String,
    delete_files: bool,
    files: PreparedInstanceDeletionFiles,
}

#[must_use = "deletion filesystem settlement must be retried"]
pub(super) struct InstanceDeletionSettlementRetry {
    guard: InstanceDeletionDropGuard,
    store: Arc<AppInstanceStore>,
    gate: OwnedMutexGuard<()>,
    instance_id: String,
    delete_files: bool,
    obligation: InstanceDeletionFilesystemObligation,
}

#[must_use = "deletion marker clear must be retried"]
pub(super) struct InstanceDeletionMarkerClearRetry {
    guard: InstanceDeletionDropGuard,
    store: Arc<AppInstanceStore>,
    gate: OwnedMutexGuard<()>,
    instance_id: String,
    delete_files: bool,
    candidate: InstanceRegistrySnapshot,
    write: InstanceDeletionMarkerWrite,
}

enum InstanceDeletionMarkerWrite {
    Prepare { encoded: Option<Vec<u8>> },
    Retry { revision: u64 },
}

pub(super) struct SettledInstanceDeletion {
    _store: Arc<AppInstanceStore>,
    _gate: OwnedMutexGuard<()>,
    instance_id: String,
    delete_files: bool,
}

pub(super) struct AbortedInstanceDeletion {
    _store: Arc<AppInstanceStore>,
    _gate: OwnedMutexGuard<()>,
    instance_id: String,
}

#[must_use = "deletion filesystem settlement must be consumed"]
pub(super) enum InstanceDeletionFilesystemSettlement {
    Aborted(AbortedInstanceDeletion),
    Settled(SettledInstanceDeletion),
}

#[must_use = "startup deletion recovery must be completed"]
pub(super) enum InstanceDeletionStartupRecovery {
    RestoreLive(InstanceDeletionSettlementRetry),
    CompletePending(CommittedInstanceDeletion),
}

#[must_use = "deletion settlement failure retains exact retry ownership"]
pub(super) enum InstanceDeletionSettlementFailure {
    Retryable {
        error: InstanceStoreError,
        retry: InstanceDeletionSettlementRetry,
    },
    Marker {
        error: InstanceStoreError,
        retry: InstanceDeletionMarkerClearRetry,
    },
}

enum PreparedInstanceDeletionFiles {
    Kept,
    Absent,
    Removed(PendingInstanceDeletion),
    Parked {
        record: PendingInstanceDeletion,
        directory: ParkedDirectory,
    },
}

impl PreparedInstanceDeletionFiles {
    fn into_restore(self) -> Option<InstanceDeletionFilesystemObligation> {
        match self {
            Self::Parked { directory, .. } => {
                Some(InstanceDeletionFilesystemObligation::RestoreParked(directory))
            }
            Self::Kept | Self::Absent | Self::Removed(_) => None,
        }
    }
}

enum InstanceDeletionFilesystemObligation {
    RestorePark(DirectoryParkObligation),
    RestoreParked(ParkedDirectory),
    Restore(DirectoryRestoreObligation),
    RemoveParked {
        record: PendingInstanceDeletion,
        directory: ParkedDirectory,
    },
    RemoveRetained {
        record: PendingInstanceDeletion,
        retained: RetainedDirectoryTreeRemoval,
    },
    Remove {
        record: PendingInstanceDeletion,
        obligation: DirectoryTreeRemovalObligation,
    },
}

enum InstanceDeletionFilesystemResolution {
    Restored,
    Removed(PendingInstanceDeletion),
    Retained {
        error: io::Error,
        obligation: InstanceDeletionFilesystemObligation,
    },
}

enum InstanceDirectoryParkAttempt {
    Prepared(PreparedInstanceDeletionFiles),
    Retry {
        error: io::Error,
        obligation: DirectoryParkObligation,
    },
}

struct InstanceDeletionTopology {
    canonical: Option<Directory>,
    tombstone: Option<Directory>,
}

struct InstanceDirectoryTopologyProof {
    instances: Directory,
    entries: Vec<DirectoryEntry>,
    bindings: HashMap<LeafNameEquivalenceKey, usize>,
}

impl InstanceDirectoryTopologyProof {
    fn classify(
        &self,
        record: &PendingInstanceDeletion,
    ) -> Result<InstanceDeletionTopology, InstanceStoreError> {
        let canonical_name = LeafName::new(record.instance_id.clone()).map_err(|_| {
            invalid_instance_deletion_topology("instance deletion id is not a native leaf")
        })?;
        let tombstone_name = LeafName::new(record.tombstone_name.clone()).map_err(|_| {
            invalid_instance_deletion_topology(
                "instance deletion tombstone is not a native leaf",
            )
        })?;
        let canonical = self.open_exact_directory(&canonical_name)?;
        let tombstone = self.open_exact_directory(&tombstone_name)?;
        Ok(InstanceDeletionTopology {
            canonical,
            tombstone,
        })
    }

    fn open_exact_directory(
        &self,
        name: &LeafName,
    ) -> Result<Option<Directory>, InstanceStoreError> {
        let mut matched = None;
        for key in leaf_name_equivalence_keys(name.as_os_str()) {
            let Some(index) = self.bindings.get(&key).copied() else {
                continue;
            };
            if matched.is_some_and(|matched| matched != index) {
                return Err(invalid_instance_deletion_topology(
                    "instance directory has conflicting portable bindings",
                ));
            }
            matched = Some(index);
        }
        let Some(index) = matched else {
            return Ok(None);
        };
        let entry = self.entries.get(index).unwrap_or_else(|| std::process::abort());
        if entry.name() != name.as_os_str() || entry.kind() != EntryKind::Directory {
            return Err(invalid_instance_deletion_topology(
                "instance directory binding has an alias or wrong kind",
            ));
        }
        self.instances
            .open_observed_directory(entry)
            .map(Some)
            .map_err(InstanceStoreError::Persistence)
    }
}

fn prove_instance_directory_topology(
    instances: Directory,
    snapshot: &InstanceRegistrySnapshot,
) -> Result<InstanceDirectoryTopologyProof, InstanceStoreError> {
    let mut allowed_tombstones = snapshot
        .pending_deletions
        .iter()
        .map(|pending| pending.tombstone_name.clone())
        .collect::<Vec<_>>();
    for instance in &snapshot.instances {
        let pending = PendingInstanceDeletion::new(
            instance.id.clone(),
            instance.created_at.clone(),
        )?;
        allowed_tombstones.push(pending.tombstone_name);
    }
    let mut allowed_bindings = HashMap::with_capacity(allowed_tombstones.len().saturating_mul(2));
    for (index, allowed) in allowed_tombstones.iter().enumerate() {
        for key in leaf_name_equivalence_keys(std::ffi::OsStr::new(allowed)) {
            if allowed_bindings
                .insert(key, index)
                .is_some_and(|other| other != index)
            {
                return Err(invalid_instance_deletion_topology(
                    "instance registry contains portable-equivalent tombstone names",
                ));
            }
        }
    }
    let listing = instances
        .entries(MAX_DIRECTORY_LIST_ENTRIES)
        .map_err(InstanceStoreError::Persistence)?;
    if listing.state() != DirectoryListingState::Complete {
        return Err(invalid_instance_deletion_topology(
            "instance directory sibling proof was truncated",
        ));
    }

    let entries = listing.entries().to_vec();
    let mut bindings = HashMap::with_capacity(entries.len().saturating_mul(2));
    let mut recognized_tombstones = 0_usize;
    for (index, entry) in entries.iter().enumerate() {
        let Some(utf8_name) = entry.utf8_name() else {
            return Err(invalid_instance_deletion_topology(
                "instance directory contains a non-Unicode sibling name",
            ));
        };
        LeafName::new(entry.name().to_os_string()).map_err(|_| {
            invalid_instance_deletion_topology(
                "instance directory contains an invalid sibling name",
            )
        })?;

        let entry_keys = leaf_name_equivalence_keys(entry.name());
        for key in &entry_keys {
            if bindings
                .insert(key.clone(), index)
                .is_some_and(|other| other != index)
            {
                return Err(invalid_instance_deletion_topology(
                    "instance directory contains portable-equivalent sibling aliases",
                ));
            }
        }

        let mut recognized_tombstone = None;
        for key in &entry_keys {
            let Some(allowed) = allowed_bindings.get(key).copied() else {
                continue;
            };
            if recognized_tombstone.is_some_and(|recognized| recognized != allowed) {
                return Err(invalid_instance_deletion_topology(
                    "instance tombstone sibling has conflicting portable bindings",
                ));
            }
            recognized_tombstone = Some(allowed);
        }
        if let Some(allowed) = recognized_tombstone {
            if entry.name() != std::ffi::OsStr::new(&allowed_tombstones[allowed])
                || entry.kind() != EntryKind::Directory
            {
                return Err(invalid_instance_deletion_topology(
                    "instance tombstone sibling has an alias or wrong kind",
                ));
            }
        }
        if utf8_name
            .to_ascii_lowercase()
            .starts_with(INSTANCE_TOMBSTONE_NAME_PREFIX)
            && recognized_tombstone.is_none()
        {
            return Err(invalid_instance_deletion_topology(
                "instance directory contains an unrecognized tombstone sibling",
            ));
        }
        if recognized_tombstone.is_some() {
            recognized_tombstones += 1;
            if recognized_tombstones > 1 {
                return Err(invalid_instance_deletion_topology(
                    "instance directory contains more than one recognized tombstone",
                ));
            }
        }
    }

    Ok(InstanceDirectoryTopologyProof {
        instances,
        entries,
        bindings,
    })
}

async fn settle_instance_deletion_filesystem(
    obligation: InstanceDeletionFilesystemObligation,
) -> InstanceDeletionFilesystemResolution {
    tokio::task::spawn_blocking(move || match obligation {
        InstanceDeletionFilesystemObligation::RestorePark(obligation) => {
            match obligation.restore() {
                DirectoryParkResolution::NoEffect(_) => {
                    InstanceDeletionFilesystemResolution::Restored
                }
                DirectoryParkResolution::Parked(directory) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: io::Error::other(
                            "instance directory park settled and still requires restoration",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::RestoreParked(directory),
                    }
                }
                DirectoryParkResolution::Indeterminate(obligation) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: retained_instance_deletion_error(
                            obligation.error(),
                            "instance directory park restoration remains unsettled",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::RestorePark(obligation),
                    }
                }
            }
        }
        InstanceDeletionFilesystemObligation::RestoreParked(directory) => {
            match directory.restore() {
                DirectoryRestoreOutcome::Restored(_) => {
                    InstanceDeletionFilesystemResolution::Restored
                }
                DirectoryRestoreOutcome::NoEffect { error, parked } => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error,
                        obligation: InstanceDeletionFilesystemObligation::RestoreParked(parked),
                    }
                }
                DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: retained_instance_deletion_error(
                            obligation.error(),
                            "instance directory restoration remains unsettled",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::Restore(obligation),
                    }
                }
            }
        }
        InstanceDeletionFilesystemObligation::Restore(obligation) => {
            match obligation.reconcile() {
                DirectoryRestoreResolution::Restored(_) => {
                    InstanceDeletionFilesystemResolution::Restored
                }
                DirectoryRestoreResolution::NoEffect(parked) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: io::Error::other("instance directory remains parked"),
                        obligation: InstanceDeletionFilesystemObligation::RestoreParked(parked),
                    }
                }
                DirectoryRestoreResolution::Indeterminate(obligation) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: retained_instance_deletion_error(
                            obligation.error(),
                            "instance directory restoration remains indeterminate",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::Restore(obligation),
                    }
                }
            }
        }
        InstanceDeletionFilesystemObligation::RemoveParked { record, directory } => {
            match directory.remove_tree() {
                DirectoryTreeRemovalOutcome::Removed => {
                    InstanceDeletionFilesystemResolution::Removed(record)
                }
                DirectoryTreeRemovalOutcome::Retained { error, retained } => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error,
                        obligation: InstanceDeletionFilesystemObligation::RemoveRetained {
                            record,
                            retained,
                        },
                    }
                }
                DirectoryTreeRemovalOutcome::Indeterminate(obligation) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: retained_instance_deletion_error(
                            obligation.error(),
                            "instance tombstone removal remains indeterminate",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::Remove {
                            record,
                            obligation,
                        },
                    }
                }
            }
        }
        InstanceDeletionFilesystemObligation::RemoveRetained { record, retained } => {
            match retained.retry() {
                DirectoryTreeRemovalOutcome::Removed => {
                    InstanceDeletionFilesystemResolution::Removed(record)
                }
                DirectoryTreeRemovalOutcome::Retained { error, retained } => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error,
                        obligation: InstanceDeletionFilesystemObligation::RemoveRetained {
                            record,
                            retained,
                        },
                    }
                }
                DirectoryTreeRemovalOutcome::Indeterminate(obligation) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: retained_instance_deletion_error(
                            obligation.error(),
                            "instance tombstone removal remains indeterminate",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::Remove {
                            record,
                            obligation,
                        },
                    }
                }
            }
        }
        InstanceDeletionFilesystemObligation::Remove { record, obligation } => {
            match obligation.reconcile() {
                DirectoryTreeRemovalResolution::Removed => {
                    InstanceDeletionFilesystemResolution::Removed(record)
                }
                DirectoryTreeRemovalResolution::Indeterminate(obligation) => {
                    InstanceDeletionFilesystemResolution::Retained {
                        error: retained_instance_deletion_error(
                            obligation.error(),
                            "instance tombstone removal remains indeterminate",
                        ),
                        obligation: InstanceDeletionFilesystemObligation::Remove {
                            record,
                            obligation,
                        },
                    }
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| std::process::abort())
}

impl InstanceDeletionPersistenceFailure {
    pub(super) fn error(&self) -> &InstanceStoreError {
        match self {
            Self::PreAcceptance { error, .. } | Self::Retryable { error, .. } => error,
        }
    }
}

impl InstanceDeletionPreparationFailure {
    pub(super) fn error(&self) -> &InstanceStoreError {
        match self {
            Self::Refused(error) | Self::Retryable { error, .. } => error,
        }
    }
}

impl PreparedInstanceDeletion {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn deletes_files(&self) -> bool {
        self.delete_files
    }

    pub(super) async fn persist(
        mut self,
    ) -> Result<CommittedInstanceDeletion, InstanceDeletionPersistenceFailure> {
        let ticket = match self.store.persistence.writer.accept_encoded(
            self.encoded.clone(),
            WriteUrgency::Immediate,
        ) {
            Ok(ticket) => ticket,
            Err(error) => {
                return Err(InstanceDeletionPersistenceFailure::PreAcceptance {
                    error: instance_persistence_error(error),
                    prepared: self,
                });
            }
        };
        let revision = ticket.revision().get();
        match ticket.persisted().await {
            Ok(committed) => {
                assert!(
                    committed.get() >= revision,
                    "instance deletion persistence acknowledged an older revision"
                );
                self.store
                    .state
                    .lock()
                    .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
                    .visible = self.candidate;
                let next = CommittedInstanceDeletion {
                    guard: InstanceDeletionDropGuard::armed(),
                    store: self.store,
                    gate: self.gate,
                    instance_id: self.instance_id,
                    delete_files: self.delete_files,
                    files: self.files,
                };
                self.guard.disarm();
                Ok(next)
            }
            Err(error) => {
                let failure = InstanceDeletionPersistenceFailure::Retryable {
                    error: instance_persistence_error(error),
                    retry: InstanceDeletionPersistenceRetry {
                        guard: InstanceDeletionDropGuard::armed(),
                        store: self.store,
                        gate: self.gate,
                        revision,
                        candidate: self.candidate,
                        instance_id: self.instance_id,
                        delete_files: self.delete_files,
                        files: self.files,
                    },
                };
                self.guard.disarm();
                Err(failure)
            }
        }
    }

    pub(super) async fn restore(
        self,
    ) -> Result<AbortedInstanceDeletion, InstanceDeletionSettlementFailure> {
        let Self {
            mut guard,
            store,
            gate,
            instance_id,
            files,
            ..
        } = self;
        let Some(obligation) = files.into_restore() else {
            let aborted = AbortedInstanceDeletion {
                _store: store,
                _gate: gate,
                instance_id,
            };
            guard.disarm();
            return Ok(aborted);
        };
        match settle_instance_deletion_filesystem(obligation).await {
            InstanceDeletionFilesystemResolution::Restored => {
                let aborted = AbortedInstanceDeletion {
                    _store: store,
                    _gate: gate,
                    instance_id,
                };
                guard.disarm();
                Ok(aborted)
            }
            InstanceDeletionFilesystemResolution::Removed(_) => std::process::abort(),
            InstanceDeletionFilesystemResolution::Retained { error, obligation } => {
                let failure = InstanceDeletionSettlementFailure::Retryable {
                    error: InstanceStoreError::Persistence(error),
                    retry: InstanceDeletionSettlementRetry {
                        guard: InstanceDeletionDropGuard::armed(),
                        store,
                        gate,
                        instance_id,
                        delete_files: false,
                        obligation,
                    },
                };
                guard.disarm();
                Err(failure)
            }
        }
    }
}

impl InstanceDeletionPreparationRetry {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) async fn retry(
        self,
    ) -> Result<PreparedInstanceDeletion, InstanceDeletionPreparationFailure> {
        let InstanceDeletionPreparationRetry {
            mut guard,
            store,
            gate,
            candidate,
            encoded,
            instance_id,
            delete_files,
            pending,
            obligation,
        } = self;
        let resolution = tokio::task::spawn_blocking(move || obligation.reconcile())
            .await
            .unwrap_or_else(|_| std::process::abort());
        match resolution {
            DirectoryParkResolution::Parked(directory) => {
                let prepared = PreparedInstanceDeletion {
                    guard: InstanceDeletionDropGuard::armed(),
                    store,
                    gate,
                    candidate,
                    encoded,
                    instance_id,
                    delete_files,
                    files: PreparedInstanceDeletionFiles::Parked {
                        record: pending,
                        directory,
                    },
                };
                guard.disarm();
                Ok(prepared)
            }
            DirectoryParkResolution::NoEffect(_) => {
                guard.disarm();
                Err(InstanceDeletionPreparationFailure::Refused(
                    InstanceStoreError::Persistence(
                        io::Error::other("instance directory park did not take effect"),
                    ),
                ))
            }
            DirectoryParkResolution::Indeterminate(obligation) => {
                let error = retained_instance_deletion_error(
                    obligation.error(),
                    "instance directory park remains unsettled",
                );
                let failure = InstanceDeletionPreparationFailure::Retryable {
                    error: InstanceStoreError::Persistence(error),
                    retry: InstanceDeletionPreparationRetry {
                        guard: InstanceDeletionDropGuard::armed(),
                        store,
                        gate,
                        candidate,
                        encoded,
                        instance_id,
                        delete_files,
                        pending,
                        obligation,
                    },
                };
                guard.disarm();
                Err(failure)
            }
        }
    }
}

impl InstanceDeletionPersistenceRetry {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) async fn retry(
        mut self,
    ) -> Result<CommittedInstanceDeletion, InstanceDeletionPersistenceFailure> {
        let ticket = match self.store.persistence.writer.retry() {
            Ok(ticket) => ticket,
            Err(error) => {
                return Err(InstanceDeletionPersistenceFailure::Retryable {
                    error: instance_persistence_error(error),
                    retry: self,
                });
            }
        };
        assert_eq!(
            ticket.revision().get(),
            self.revision,
            "instance deletion retry revision diverged from its retained carrier"
        );
        match ticket.persisted().await {
            Ok(committed) => {
                assert!(
                    committed.get() >= self.revision,
                    "instance deletion retry acknowledged an older revision"
                );
                self.store
                    .state
                    .lock()
                    .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
                    .visible = self.candidate;
                let next = CommittedInstanceDeletion {
                    guard: InstanceDeletionDropGuard::armed(),
                    store: self.store,
                    gate: self.gate,
                    instance_id: self.instance_id,
                    delete_files: self.delete_files,
                    files: self.files,
                };
                self.guard.disarm();
                Ok(next)
            }
            Err(error) => Err(InstanceDeletionPersistenceFailure::Retryable {
                error: instance_persistence_error(error),
                retry: self,
            }),
        }
    }
}

impl CommittedInstanceDeletion {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn deletes_files(&self) -> bool {
        self.delete_files
    }

    pub(super) async fn settle_files(
        self,
    ) -> Result<SettledInstanceDeletion, InstanceDeletionSettlementFailure> {
        let Self {
            mut guard,
            store,
            gate,
            instance_id,
            delete_files,
            files,
        } = self;
        let obligation = match files {
            PreparedInstanceDeletionFiles::Kept | PreparedInstanceDeletionFiles::Absent => {
                let settled = SettledInstanceDeletion {
                    _store: store,
                    _gate: gate,
                    instance_id,
                    delete_files,
                };
                guard.disarm();
                return Ok(settled);
            }
            PreparedInstanceDeletionFiles::Removed(record) => {
                let result = store
                    .finish_removed_instance_deletion(instance_id, delete_files, record, gate)
                    .await;
                guard.disarm();
                return result;
            }
            PreparedInstanceDeletionFiles::Parked { record, directory } => {
                InstanceDeletionFilesystemObligation::RemoveParked { record, directory }
            }
        };
        match settle_instance_deletion_filesystem(obligation).await {
            InstanceDeletionFilesystemResolution::Removed(record) => {
                let result = store
                    .finish_removed_instance_deletion(
                        instance_id,
                        delete_files,
                        record,
                        gate,
                    )
                    .await;
                guard.disarm();
                result
            }
            InstanceDeletionFilesystemResolution::Restored => std::process::abort(),
            InstanceDeletionFilesystemResolution::Retained { error, obligation } => {
                let failure = InstanceDeletionSettlementFailure::Retryable {
                    error: InstanceStoreError::Persistence(error),
                    retry: InstanceDeletionSettlementRetry {
                        guard: InstanceDeletionDropGuard::armed(),
                        store,
                        gate,
                        instance_id,
                        delete_files,
                        obligation,
                    },
                };
                guard.disarm();
                Err(failure)
            }
        }
    }
}

impl SettledInstanceDeletion {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn deleted_files(&self) -> bool {
        self.delete_files
    }
}

impl InstanceDeletionSettlementFailure {
    pub(super) fn error(&self) -> &InstanceStoreError {
        match self {
            Self::Retryable { error, .. } | Self::Marker { error, .. } => error,
        }
    }
}

impl InstanceDeletionSettlementRetry {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) async fn retry(
        self,
    ) -> Result<InstanceDeletionFilesystemSettlement, InstanceDeletionSettlementFailure> {
        let Self {
            mut guard,
            store,
            gate,
            instance_id,
            delete_files,
            obligation,
        } = self;
        match settle_instance_deletion_filesystem(obligation).await {
            InstanceDeletionFilesystemResolution::Restored => {
                let settlement = InstanceDeletionFilesystemSettlement::Aborted(
                    AbortedInstanceDeletion {
                        _store: store,
                        _gate: gate,
                        instance_id,
                    },
                );
                guard.disarm();
                Ok(settlement)
            }
            InstanceDeletionFilesystemResolution::Removed(record) => {
                let result = store
                    .finish_removed_instance_deletion(instance_id, delete_files, record, gate)
                    .await
                    .map(InstanceDeletionFilesystemSettlement::Settled);
                guard.disarm();
                result
            }
            InstanceDeletionFilesystemResolution::Retained { error, obligation } => {
                let failure = InstanceDeletionSettlementFailure::Retryable {
                    error: InstanceStoreError::Persistence(error),
                    retry: InstanceDeletionSettlementRetry {
                        guard: InstanceDeletionDropGuard::armed(),
                        store,
                        gate,
                        instance_id,
                        delete_files,
                        obligation,
                    },
                };
                guard.disarm();
                Err(failure)
            }
        }
    }
}

impl InstanceDeletionMarkerClearRetry {
    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) async fn retry(
        mut self,
    ) -> Result<SettledInstanceDeletion, InstanceDeletionSettlementFailure> {
        let ticket = match &mut self.write {
            InstanceDeletionMarkerWrite::Prepare { encoded } => {
                if encoded.is_none() {
                    let (candidate, result) =
                        encode_instance_registry_retained(self.candidate).await;
                    self.candidate = candidate;
                    match result {
                        Ok(bytes) => *encoded = Some(bytes),
                        Err(error) => {
                            return Err(InstanceDeletionSettlementFailure::Marker {
                                error,
                                retry: self,
                            });
                        }
                    }
                }
                match self.store.persistence.writer.accept_encoded(
                    encoded
                        .as_ref()
                        .expect("marker-clear encoding is retained")
                        .clone(),
                    WriteUrgency::Immediate,
                ) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        return Err(InstanceDeletionSettlementFailure::Marker {
                            error: instance_persistence_error(error),
                            retry: self,
                        });
                    }
                }
            }
            InstanceDeletionMarkerWrite::Retry { revision } => {
                let ticket = match self.store.persistence.writer.retry() {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        return Err(InstanceDeletionSettlementFailure::Marker {
                            error: instance_persistence_error(error),
                            retry: self,
                        });
                    }
                };
                assert_eq!(
                    ticket.revision().get(),
                    *revision,
                    "instance deletion marker retry revision diverged"
                );
                ticket
            }
        };
        let revision = ticket.revision().get();
        match ticket.persisted().await {
            Ok(committed) => {
                assert!(
                    committed.get() >= revision,
                    "instance deletion marker clear acknowledged an older revision"
                );
                self.store
                    .state
                    .lock()
                    .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
                    .visible = self.candidate;
                let settled = SettledInstanceDeletion {
                    _store: self.store,
                    _gate: self.gate,
                    instance_id: self.instance_id,
                    delete_files: self.delete_files,
                };
                self.guard.disarm();
                Ok(settled)
            }
            Err(error) => {
                self.write = InstanceDeletionMarkerWrite::Retry { revision };
                Err(InstanceDeletionSettlementFailure::Marker {
                    error: instance_persistence_error(error),
                    retry: self,
                })
            }
        }
    }
}

struct ManagedInstanceContentRoot {
    incarnation: InstanceLifecycleIncarnation,
    root: Option<ManagedTreeRoot>,
    retirement: Option<Arc<ManagedTreeRetirement>>,
    settlement: Arc<AsyncMutex<()>>,
}

struct InstanceContentRootReservation<'a> {
    count: &'a AtomicUsize,
    committed: bool,
}

impl InstanceContentRootReservation<'_> {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for InstanceContentRootReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl ManagedInstanceContentRoot {
    fn active(
        incarnation: InstanceLifecycleIncarnation,
        root: ManagedTreeRoot,
    ) -> Self {
        Self {
            incarnation,
            root: Some(root),
            retirement: None,
            settlement: Arc::new(AsyncMutex::new(())),
        }
    }

    fn begin_retirement(
        &mut self,
    ) -> (Arc<ManagedTreeRetirement>, Arc<AsyncMutex<()>>) {
        if let Some(retirement) = &self.retirement {
            return (Arc::clone(retirement), Arc::clone(&self.settlement));
        }
        let root = self
            .root
            .take()
            .unwrap_or_else(|| std::process::abort());
        let retirement = Arc::new(root.begin_retirement());
        self.retirement = Some(Arc::clone(&retirement));
        (retirement, Arc::clone(&self.settlement))
    }
}

struct PendingInstanceRegistryCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: InstanceRegistrySnapshot,
}

struct InstanceRegistryCloseTransition {
    closed: Arc<AtomicBool>,
    _gate: OwnedMutexGuard<()>,
    finished: bool,
}

impl InstanceRegistryCloseTransition {
    fn begin(closed: Arc<AtomicBool>, gate: OwnedMutexGuard<()>) -> Self {
        closed.store(true, Ordering::Release);
        Self {
            closed,
            _gate: gate,
            finished: false,
        }
    }

    fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for InstanceRegistryCloseTransition {
    fn drop(&mut self) {
        if !self.finished {
            self.closed.store(false, Ordering::Release);
        }
    }
}

pub struct AppInstanceStore {
    paths: AppPaths,
    instance_directory_issuer: Mutex<Option<Directory>>,
    instance_content_admission: Arc<RwLock<()>>,
    instance_content_roots: Mutex<HashMap<String, ManagedInstanceContentRoot>>,
    instance_content_root_count: AtomicUsize,
    root_session: Arc<AppRootSession>,
    mutation_allowed: bool,
    state: Arc<Mutex<InstanceRegistryState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    closed: Arc<AtomicBool>,
    persistence: InstanceRegistryPersistence,
}

#[derive(Default)]
pub(crate) struct InstanceUpdate {
    pub(crate) name: Option<String>,
    pub(crate) expected_version_id: Option<String>,
    pub(crate) art_seed: Option<u32>,
    pub(crate) max_memory_mb: Option<i32>,
    pub(crate) min_memory_mb: Option<i32>,
    pub(crate) java_path: Option<String>,
    pub(crate) window_width: Option<i32>,
    pub(crate) window_height: Option<i32>,
    pub(crate) jvm_preset: Option<String>,
    pub(crate) performance_mode: Option<String>,
    pub(crate) extra_jvm_args: Option<String>,
    pub(crate) icon: Option<String>,
    pub(crate) accent: Option<String>,
}

impl AppInstanceStore {
    pub(crate) fn claim(
        source: &InstanceStore,
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, InstanceStoreError> {
        let paths = source.paths().clone();
        admit_instance_source(source, &directory)?;
        let persistence = InstanceRegistryPersistence::claim(directory)?;
        Self::from_parts(
            paths,
            Arc::clone(source.root_session()),
            source.current(),
            source.mutation_allowed(),
            persistence,
        )
    }

    #[cfg(test)]
    pub(crate) fn claim_with_coordinator(
        source: &InstanceStore,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, InstanceStoreError> {
        let paths = source.paths().clone();
        let root_session = Arc::clone(source.root_session());
        let directory = AnchoredRecordDirectory::from_directory(
            root_session.clone(),
            root_session.root_directory().map_err(instance_persistence_error)?,
        );
        admit_instance_source(source, &directory)?;
        let persistence =
            InstanceRegistryPersistence::claim_with_coordinator(directory, coordinator)?;
        Self::from_parts(
            paths,
            Arc::clone(source.root_session()),
            source.current(),
            source.mutation_allowed(),
            persistence,
        )
    }

    fn from_parts(
        paths: AppPaths,
        root_session: Arc<AppRootSession>,
        visible: InstanceRegistrySnapshot,
        mutation_allowed: bool,
        persistence: InstanceRegistryPersistence,
    ) -> Result<Self, InstanceStoreError> {
        let directory = root_session
            .prepare_instances_directory()
            .map_err(InstanceStoreError::Persistence)?;
        Ok(Self {
            paths,
            instance_directory_issuer: Mutex::new(Some(directory)),
            instance_content_admission: Arc::new(RwLock::new(())),
            instance_content_roots: Mutex::new(HashMap::new()),
            instance_content_root_count: AtomicUsize::new(0),
            root_session,
            mutation_allowed,
            state: Arc::new(Mutex::new(InstanceRegistryState {
                visible,
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            closed: Arc::new(AtomicBool::new(false)),
            persistence,
        })
    }

    pub fn current(&self) -> InstanceRegistrySnapshot {
        self.state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .visible
            .clone()
    }

    pub fn list(&self) -> Vec<Instance> {
        self.current().instances
    }

    pub fn get(&self, id: &str) -> Option<Instance> {
        self.state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .visible
            .instances
            .iter()
            .find(|instance| instance.id == id)
            .cloned()
    }

    pub fn last_instance_id(&self) -> Option<String> {
        let id = self
            .state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .visible
            .last_instance_id
            .clone();
        (!id.is_empty()).then_some(id)
    }

    pub fn game_dir(&self, id: &str) -> PathBuf {
        self.paths.instances_dir().join(id)
    }

    pub(crate) fn mods_directory(&self, instance_id: &str) -> io::Result<Directory> {
        let expected = self.get(instance_id).filter(|instance| {
            is_canonical_instance_id(&instance.id) && instance.id == instance_id
        });
        let Some(expected) = expected else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "registered instance does not exist",
            ));
        };
        let instances = self.root_session.prepare_instances_directory()?;
        let instance_name = LeafName::new(instance_id).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "instance id is not a native leaf")
        })?;
        let instance = instances.open_directory(&instance_name)?;
        let mods_name = LeafName::new("mods").expect("fixed mods directory leaf is valid");
        let mods = instance.open_directory(&mods_name)?;
        if self.get(instance_id).as_ref() != Some(&expected) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "registered instance changed while opening its mods directory",
            ));
        }
        Ok(mods)
    }

    fn reserve_instance_content_root(&self) -> io::Result<InstanceContentRootReservation<'_>> {
        self.instance_content_root_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < INSTANCE_CONTENT_ROOT_LIMIT).then_some(count + 1)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "managed instance content root capacity is temporarily exhausted",
                )
            })?;
        Ok(InstanceContentRootReservation {
            count: &self.instance_content_root_count,
            committed: false,
        })
    }

    pub(super) fn managed_game_directory(
        self: &Arc<Self>,
        expected: &Instance,
        incarnation: &InstanceLifecycleIncarnation,
        _admission: &OwnedRwLockReadGuard<()>,
    ) -> io::Result<(ManagedTreeOperation, ManagedTreeDirectory)> {
        if !is_canonical_instance_id(&expected.id)
            || self.get(&expected.id).as_ref() != Some(expected)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "registered instance generation does not exist",
            ));
        }
        self.settle_retained_instance_content_root(&expected.id, incarnation)?;
        let root_reservation = self.reserve_instance_content_root()?;
        let instances_directory = {
            let owner = self
                .instance_directory_issuer
                .lock()
                .map_err(|_| io::Error::other("managed instance directory lock was poisoned"))?;
            if self.closed.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "instance registry persistence is closed",
                ));
            }
            owner.as_ref().cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "managed instance directory owner is retired",
                )
            })?
        };
        instances_directory.identity()?;
        let instance_name = LeafName::new(&expected.id).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "instance id is not a native leaf")
        })?;
        let instance_directory = instances_directory.open_directory(&instance_name)?;
        let effects = instance_directory.create_effect_owner()?;
        let root = ManagedTreeRoot::from_directory(instance_directory, effects)?;
        let operation = root.try_acquire()?;
        let directory = operation.directory()?;
        {
            let mut roots = self.instance_content_roots.lock().map_err(|_| {
                io::Error::other("managed instance content root pool lock was poisoned")
            })?;
            if roots.contains_key(&expected.id) {
                drop(roots);
                drop((directory, operation));
                let retirement = root.begin_retirement();
                if !matches!(retirement.try_drain_and_settle(), Ok(Some(()))) {
                    std::process::abort();
                }
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "instance content root admission overlapped another context",
                ));
            }
            roots.insert(
                expected.id.clone(),
                ManagedInstanceContentRoot::active(incarnation.clone(), root),
            );
            root_reservation.commit();
        }
        if let Err(error) =
            self.require_registered_instance_unchanged(&expected.id, expected)
        {
            drop((directory, operation));
            self.release_managed_game_directory(&expected.id, incarnation);
            return Err(error);
        }
        Ok((operation, directory))
    }

    pub(super) async fn acquire_instance_content_admission(
        &self,
    ) -> io::Result<OwnedRwLockReadGuard<()>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_instance_registry_io_error());
        }
        let admission = Arc::clone(&self.instance_content_admission)
            .read_owned()
            .await;
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_instance_registry_io_error());
        }
        Ok(admission)
    }

    pub(super) fn release_managed_game_directory(
        self: &Arc<Self>,
        instance_id: &str,
        incarnation: &InstanceLifecycleIncarnation,
    ) {
        let (retirement, settlement) = {
            let mut roots = self
                .instance_content_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(root) = roots.get_mut(instance_id) else {
                std::process::abort();
            };
            if !root.incarnation.same(incarnation) {
                std::process::abort();
            }
            root.begin_retirement()
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = Arc::clone(self);
        let instance_id = instance_id.to_string();
        let incarnation = incarnation.clone();
        runtime.spawn(async move {
            match settle_instance_content_retirement(Arc::clone(&retirement), settlement).await {
                Ok(()) => store.remove_settled_instance_content_root(
                    &instance_id,
                    &incarnation,
                    &retirement,
                ),
                Err(error) => tracing::warn!(
                    error = %error,
                    "managed instance content root cleanup was retained for retry"
                ),
            }
        });
    }

    #[cfg(test)]
    async fn wait_for_instance_content_root_release(&self, instance_id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !self
                    .instance_content_roots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(instance_id)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("managed instance content root release timed out");
    }

    fn instance_content_retirement(
        &self,
        instance_id: &str,
        incarnation: &InstanceLifecycleIncarnation,
    ) -> io::Result<Option<(Arc<ManagedTreeRetirement>, Arc<AsyncMutex<()>>)>> {
        let mut roots = self.instance_content_roots.lock().map_err(|_| {
            io::Error::other("managed instance content root pool lock was poisoned")
        })?;
        let Some(root) = roots.get_mut(instance_id) else {
            return Ok(None);
        };
        if !root.incarnation.same(incarnation) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "instance content root belongs to another lifecycle incarnation",
            ));
        }
        Ok(Some(root.begin_retirement()))
    }

    pub(super) async fn retire_managed_game_directory(
        &self,
        instance_id: &str,
        incarnation: &InstanceLifecycleIncarnation,
    ) -> io::Result<()> {
        let Some((retirement, settlement)) =
            self.instance_content_retirement(instance_id, incarnation)?
        else {
            return Ok(());
        };
        settle_instance_content_retirement(Arc::clone(&retirement), settlement).await?;
        self.remove_settled_instance_content_root(instance_id, incarnation, &retirement);
        Ok(())
    }

    fn settle_retained_instance_content_root(
        &self,
        instance_id: &str,
        incarnation: &InstanceLifecycleIncarnation,
    ) -> io::Result<()> {
        let (retirement, settlement) = {
            let roots = self.instance_content_roots.lock().map_err(|_| {
                io::Error::other("managed instance content root pool lock was poisoned")
            })?;
            let Some(root) = roots.get(instance_id) else {
                return Ok(());
            };
            if !root.incarnation.same(incarnation) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "retained instance content root belongs to another lifecycle incarnation",
                ));
            }
            let retirement = root.retirement.as_ref().cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "instance content root is already active",
                )
            })?;
            (retirement, Arc::clone(&root.settlement))
        };
        let _settlement = settlement.blocking_lock_owned();
        match retirement.try_drain_and_settle()? {
            Some(()) => {
                self.remove_settled_instance_content_root(
                    instance_id,
                    incarnation,
                    &retirement,
                );
                Ok(())
            }
            None => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "retained instance content root still has active operations",
            )),
        }
    }

    fn remove_settled_instance_content_root(
        &self,
        instance_id: &str,
        incarnation: &InstanceLifecycleIncarnation,
        retirement: &Arc<ManagedTreeRetirement>,
    ) {
        let mut roots = self
            .instance_content_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = roots.get(instance_id).is_some_and(|root| {
            root.incarnation.same(incarnation)
                && root
                    .retirement
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, retirement))
        });
        if remove {
            roots.remove(instance_id);
            if self
                .instance_content_root_count
                .fetch_sub(1, Ordering::AcqRel)
                == 0
            {
                std::process::abort();
            }
        }
    }

    async fn close_instance_content_roots(
        &self,
    ) -> io::Result<OwnedRwLockWriteGuard<()>> {
        let admission = Arc::clone(&self.instance_content_admission)
            .write_owned()
            .await;
        let retirements = {
            let mut roots = self.instance_content_roots.lock().map_err(|_| {
                io::Error::other("managed instance content root pool lock was poisoned")
            })?;
            roots
                .iter_mut()
                .map(|(instance_id, root)| {
                    (
                        instance_id.clone(),
                        root.incarnation.clone(),
                        root.begin_retirement(),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (instance_id, incarnation, (retirement, settlement)) in retirements {
            settle_instance_content_retirement(Arc::clone(&retirement), settlement).await?;
            self.remove_settled_instance_content_root(
                &instance_id,
                &incarnation,
                &retirement,
            );
        }
        Ok(admission)
    }

    fn require_no_instance_content_root(&self, instance_id: &str) -> io::Result<()> {
        let roots = self.instance_content_roots.lock().map_err(|_| {
            io::Error::other("managed instance content root pool lock was poisoned")
        })?;
        if roots.contains_key(instance_id) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "instance content root must retire before directory removal",
            ));
        }
        Ok(())
    }

    fn require_registered_instance_unchanged(
        &self,
        instance_id: &str,
        expected: &Instance,
    ) -> io::Result<()> {
        if self.get(instance_id).as_ref() != Some(expected) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "instance registry changed during managed directory admission",
            ));
        }
        Ok(())
    }

    pub(crate) async fn registered_game_dir(
        &self,
        instance: &Instance,
    ) -> Result<PathBuf, InstanceStoreError> {
        if !is_canonical_instance_id(&instance.id)
            || self.get(&instance.id).as_ref() != Some(instance)
        {
            return Err(instance_not_found_error().into());
        }
        let game_dir = self.game_dir(&instance.id);
        for path in [self.paths.instances_dir(), &game_dir] {
            let metadata = tokio::fs::symlink_metadata(path).await?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(InstanceStoreError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registered instance root is not a regular directory",
                )));
            }
        }
        if self.get(&instance.id).as_ref() != Some(instance) {
            return Err(instance_not_found_error().into());
        }
        Ok(game_dir)
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub(crate) async fn acquire_mutation(&self) -> Result<OwnedMutexGuard<()>, InstanceStoreError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_instance_registry_error());
        }
        if !self.mutation_allowed {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance registry mutation is latched after startup admission failure",
            )));
        }
        Ok(gate)
    }

    #[cfg(test)]
    async fn mutate<ResultValue, Mutation>(
        &self,
        mutation: Mutation,
    ) -> Result<ResultValue, InstanceStoreError>
    where
        ResultValue: Send + 'static,
        Mutation: FnOnce(&mut InstanceRegistrySnapshot) -> Result<ResultValue, InstanceStoreError>
            + Send
            + 'static,
    {
        let gate = self.acquire_mutation().await?;
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let result = mutation(&mut candidate)?;
        candidate.validate()?;
        if candidate == self.current() {
            drop(gate);
            return Ok(result);
        }
        self.commit(candidate, result, gate).await
    }

    pub(crate) async fn update_with_gate(
        &self,
        instance_id: String,
        update: InstanceUpdate,
        gate: OwnedMutexGuard<()>,
    ) -> Result<Instance, InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let Some(index) = candidate
            .instances
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(instance_not_found_error());
        };
        let mut instance = candidate.instances[index].clone();
        if let Some(name) = update.name.filter(|value| !value.trim().is_empty()) {
            if candidate
                .instances
                .iter()
                .any(|stored| stored.id != instance.id && stored.name == name)
            {
                return Err(instance_name_conflict_error());
            }
            instance.name = name;
        }
        if let Some(version_id) = update
            .expected_version_id
            .filter(|value| !value.trim().is_empty())
            && version_id != instance.version_id
        {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct version changes are not supported",
            )));
        }
        if let Some(value) = update.art_seed {
            instance.art_seed = value;
        }
        if let Some(value) = update.max_memory_mb {
            instance.max_memory_mb = value.max(0);
        }
        if let Some(value) = update.min_memory_mb {
            instance.min_memory_mb = value.max(0);
        }
        if let Some(value) = update.java_path {
            instance.java_path = value;
        }
        if let Some(value) = update.window_width {
            instance.window_width = value.max(0);
        }
        if let Some(value) = update.window_height {
            instance.window_height = value.max(0);
        }
        if let Some(value) = update.jvm_preset {
            instance.jvm_preset = value;
        }
        if let Some(value) = update.performance_mode {
            instance.performance_mode = value;
        }
        if let Some(value) = update.extra_jvm_args {
            instance.extra_jvm_args = value;
        }
        if let Some(value) = update.icon {
            instance.icon = value;
        }
        if let Some(value) = update.accent {
            instance.accent = value;
        }
        candidate.instances[index] = instance.clone();
        candidate.validate()?;
        if candidate == self.current() {
            drop(gate);
            return Ok(instance);
        }
        self.commit(candidate, instance, gate).await
    }

    pub(crate) async fn record_successful_launch_with_gate(
        &self,
        instance_id: String,
        last_played_at: String,
        gate: OwnedMutexGuard<()>,
    ) -> Result<(), InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let stored = candidate
            .instances
            .iter_mut()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(instance_not_found_error)?;
        stored.last_played_at = last_played_at;
        candidate.last_instance_id = instance_id;
        candidate.validate()?;
        if candidate == self.current() {
            drop(gate);
            return Ok(());
        }
        self.commit(candidate, (), gate).await
    }

    pub(crate) async fn create_with_gate(
        &self,
        mut instance: Instance,
        library_dir: Option<PathBuf>,
        gate: OwnedMutexGuard<()>,
    ) -> Result<Instance, InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let original_name = instance.name.clone();
        let original_seed =
            derive_instance_art_seed(&instance.id, &original_name, &instance.version_id);
        instance.name = available_create_name(&candidate, &original_name);
        if instance.name != original_name && instance.art_seed == original_seed {
            instance.art_seed =
                derive_instance_art_seed(&instance.id, &instance.name, &instance.version_id);
        }
        ensure_insertable(&candidate, &instance)?;
        candidate.instances.push(instance.clone());
        candidate.validate()?;
        prepare_new_instance_layout(self.paths.clone(), instance.id.clone(), library_dir).await?;
        match self.commit(candidate, instance.clone(), gate).await {
            Ok(instance) => Ok(instance),
            Err(error @ InstanceStoreError::TooLarge { .. }) => {
                Err(cleanup_failed_create(&self.paths, &instance.id, error).await)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn duplicate_with_gate(
        &self,
        source_id: String,
        target_id: String,
        requested_name: Option<String>,
        gate: OwnedMutexGuard<()>,
    ) -> Result<Instance, InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let source = candidate
            .instances
            .iter()
            .find(|instance| instance.id == source_id)
            .cloned()
            .ok_or_else(instance_not_found_error)?;
        let name = duplicate_name(&candidate, &source.name, requested_name)?;
        let mut instance = new_instance(
            target_id,
            name,
            source.version_id.clone(),
            source.icon.clone(),
            source.accent.clone(),
        );
        instance.max_memory_mb = source.max_memory_mb;
        instance.min_memory_mb = source.min_memory_mb;
        instance.java_path = source.java_path;
        instance.window_width = source.window_width;
        instance.window_height = source.window_height;
        instance.jvm_preset = source.jvm_preset;
        instance.performance_mode = source.performance_mode;
        instance.extra_jvm_args = source.extra_jvm_args;
        instance.auto_optimize = source.auto_optimize;
        candidate.instances.push(instance.clone());
        candidate.validate()?;

        duplicate_instance_files(self.paths.clone(), source_id, instance.id.clone()).await?;
        match self.commit(candidate, instance.clone(), gate).await {
            Ok(instance) => Ok(instance),
            Err(error @ InstanceStoreError::TooLarge { .. }) => {
                Err(cleanup_failed_create(&self.paths, &instance.id, error).await)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn prepare_startup_deletion_recovery_with_gate(
        self: &Arc<Self>,
        gate: OwnedMutexGuard<()>,
    ) -> Result<Option<InstanceDeletionStartupRecovery>, InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let snapshot = self.current();
        let instances = self
            .instance_directory_issuer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .ok_or_else(closed_instance_registry_error)?;
        let snapshot_for_probe = snapshot.clone();
        let probe = tokio::task::spawn_blocking(move || {
            let proof = prove_instance_directory_topology(instances.clone(), &snapshot_for_probe)?;
            let mut selected = None;
            for instance in &snapshot_for_probe.instances {
                let record = PendingInstanceDeletion::new(
                    instance.id.clone(),
                    instance.created_at.clone(),
                )?;
                let topology = proof.classify(&record)?;
                match (topology.canonical, topology.tombstone) {
                    (Some(_), Some(_)) => {
                        return Err(invalid_instance_deletion_topology(
                            "live instance has both canonical and tombstone directories",
                        ));
                    }
                    (None, Some(parked)) => {
                        if selected.is_some() {
                            return Err(invalid_instance_deletion_topology(
                                "instance startup requires more than one deletion recovery",
                            ));
                        }
                        selected = Some((record.instance_id, None, Some(parked)));
                    }
                    (Some(_), None) | (None, None) => {}
                }
            }

            for record in &snapshot_for_probe.pending_deletions {
                let topology = proof.classify(record)?;
                match (topology.canonical, topology.tombstone) {
                    (Some(_), _) => {
                        return Err(invalid_instance_deletion_topology(
                            "pending instance deletion still has a canonical directory",
                        ));
                    }
                    (None, Some(parked)) => {
                        if selected.is_some() {
                            return Err(invalid_instance_deletion_topology(
                                "instance startup requires more than one deletion recovery",
                            ));
                        }
                        selected = Some((
                            record.instance_id.clone(),
                            Some(record.clone()),
                            Some(parked),
                        ));
                    }
                    (None, None) => {
                        if selected.is_some() {
                            return Err(invalid_instance_deletion_topology(
                                "instance startup requires more than one deletion recovery",
                            ));
                        }
                        selected = Some((
                            record.instance_id.clone(),
                            Some(record.clone()),
                            None,
                        ));
                    }
                }
            }
            let Some((instance_id, pending, parked)) = selected else {
                return Ok(None);
            };
            Ok(Some((instances, instance_id, pending, parked)))
        })
        .await
        .map_err(|error| {
            InstanceStoreError::Persistence(io::Error::other(format!(
                "instance startup deletion topology task stopped: {error}"
            )))
        })??;

        let Some((instances, instance_id, pending, parked)) = probe else {
            return Ok(None);
        };
        let mut deletion_guard = InstanceDeletionDropGuard::armed();
        let parked = if let Some(parked) = parked {
            let original_name = LeafName::new(instance_id.clone()).map_err(|_| {
                invalid_instance_deletion_topology("instance recovery id is not a native leaf")
            })?;
            let parked = tokio::task::spawn_blocking(move || {
                let revision = parked
                    .revision()
                    .map_err(InstanceStoreError::Persistence)?;
                instances
                    .admit_existing_directory_park(&original_name, parked, &revision)
                    .map(Some)
                    .map_err(InstanceStoreError::Persistence)
            })
            .await
            .unwrap_or_else(|_| std::process::abort());
            match parked {
                Ok(parked) => parked,
                Err(error) => {
                    deletion_guard.disarm();
                    return Err(error);
                }
            }
        } else {
            None
        };
        let recovery = match (pending, parked) {
            (None, Some(parked)) => InstanceDeletionStartupRecovery::RestoreLive(
                InstanceDeletionSettlementRetry {
                    guard: InstanceDeletionDropGuard::armed(),
                    store: Arc::clone(self),
                    gate,
                    instance_id,
                    delete_files: false,
                    obligation: InstanceDeletionFilesystemObligation::RestoreParked(parked),
                },
            ),
            (Some(record), parked) => {
                InstanceDeletionStartupRecovery::CompletePending(CommittedInstanceDeletion {
                    guard: InstanceDeletionDropGuard::armed(),
                    store: Arc::clone(self),
                    gate,
                    instance_id,
                    delete_files: true,
                    files: match parked {
                        Some(directory) => PreparedInstanceDeletionFiles::Parked {
                            record,
                            directory,
                        },
                        None => PreparedInstanceDeletionFiles::Removed(record),
                    },
                })
            }
            (None, None) => std::process::abort(),
        };
        deletion_guard.disarm();
        Ok(Some(recovery))
    }

    pub(super) async fn prepare_delete_with_gate(
        self: &Arc<Self>,
        instance_id: String,
        delete_files: bool,
        gate: OwnedMutexGuard<()>,
    ) -> Result<PreparedInstanceDeletion, InstanceDeletionPreparationFailure> {
        self.require_no_instance_content_root(&instance_id)
            .map_err(InstanceStoreError::Persistence)?;
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        if !candidate.pending_deletions.is_empty() {
            return Err(pending_instance_deletion_recovery_error());
        }
        let Some(index) = candidate
            .instances
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(instance_not_found_error().into());
        };
        let instance = candidate.instances.remove(index);
        if candidate.last_instance_id == instance_id {
            candidate.last_instance_id.clear();
        }

        let pending = delete_files
            .then(|| PendingInstanceDeletion::new(instance.id.clone(), instance.created_at.clone()))
            .transpose()?;
        if let Some(pending) = &pending {
            candidate.pending_deletions.push(pending.clone());
        }
        candidate.validate()?;
        let (candidate, encoded) = encode_instance_registry(candidate).await?;

        let mut deletion_guard = InstanceDeletionDropGuard::armed();
        let files = if let Some(pending) = pending {
            let attempt = match self
                .prepare_instance_deletion_files(
                    instance.id.clone(),
                    pending.clone(),
                    candidate.clone(),
                )
                .await
            {
                Ok(attempt) => attempt,
                Err(error) => {
                    deletion_guard.disarm();
                    return Err(error.into());
                }
            };
            match attempt {
                InstanceDirectoryParkAttempt::Prepared(
                    PreparedInstanceDeletionFiles::Absent,
                ) => PreparedInstanceDeletionFiles::Removed(pending),
                InstanceDirectoryParkAttempt::Prepared(prepared) => prepared,
                InstanceDirectoryParkAttempt::Retry { error, obligation } => {
                    return Err(InstanceDeletionPreparationFailure::Retryable {
                        error: InstanceStoreError::Persistence(error),
                        retry: InstanceDeletionPreparationRetry {
                            guard: deletion_guard,
                            store: Arc::clone(self),
                            gate,
                            candidate,
                            encoded,
                            instance_id: instance.id,
                            delete_files,
                            pending,
                            obligation,
                        },
                    });
                }
            }
        } else {
            PreparedInstanceDeletionFiles::Kept
        };

        Ok(PreparedInstanceDeletion {
            guard: deletion_guard,
            store: Arc::clone(self),
            gate,
            candidate,
            encoded,
            instance_id: instance.id,
            delete_files,
            files,
        })
    }

    async fn prepare_instance_deletion_files(
        &self,
        instance_id: String,
        pending: PendingInstanceDeletion,
        snapshot: InstanceRegistrySnapshot,
    ) -> Result<InstanceDirectoryParkAttempt, InstanceStoreError> {
        let instances = self
            .instance_directory_issuer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .ok_or_else(closed_instance_registry_error)?;
        tokio::task::spawn_blocking(move || {
            let original_name = LeafName::new(instance_id).map_err(|_| {
                InstanceStoreError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "instance deletion id is not a native leaf",
                ))
            })?;
            let tombstone_name = LeafName::new(pending.tombstone_name.clone()).map_err(|_| {
                InstanceStoreError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "instance deletion tombstone is not a native leaf",
                ))
            })?;
            let proof = prove_instance_directory_topology(instances.clone(), &snapshot)?;
            let topology = proof.classify(&pending)?;
            let directory = match (topology.canonical, topology.tombstone) {
                (Some(_), Some(_)) => {
                    return Err(invalid_instance_deletion_topology(
                        "instance deletion has both canonical and tombstone directories",
                    ));
                }
                (None, Some(parked)) => {
                    let revision = parked
                        .revision()
                        .map_err(InstanceStoreError::Persistence)?;
                    let parked = instances
                        .admit_existing_directory_park(&original_name, parked, &revision)
                        .map_err(InstanceStoreError::Persistence)?;
                    return Ok(InstanceDirectoryParkAttempt::Prepared(
                        PreparedInstanceDeletionFiles::Parked {
                            record: pending,
                            directory: parked,
                        },
                    ));
                }
                (None, None) => {
                    return Ok(InstanceDirectoryParkAttempt::Prepared(
                        PreparedInstanceDeletionFiles::Absent,
                    ));
                }
                (Some(directory), None) => directory,
            };
            match directory.park_as(tombstone_name) {
                DirectoryParkOutcome::Parked(directory) => {
                    Ok(InstanceDirectoryParkAttempt::Prepared(
                        PreparedInstanceDeletionFiles::Parked {
                            record: pending,
                            directory,
                        },
                    ))
                }
                DirectoryParkOutcome::NoEffect { error, .. } => {
                    Err(InstanceStoreError::Persistence(error))
                }
                DirectoryParkOutcome::AppliedUnverified(obligation) => {
                    let error = retained_instance_deletion_error(
                        obligation.error(),
                        "instance directory park requires reconciliation",
                    );
                    Ok(InstanceDirectoryParkAttempt::Retry { error, obligation })
                }
            }
        })
        .await
        .unwrap_or_else(|_| std::process::abort())
    }

    async fn finish_removed_instance_deletion(
        self: &Arc<Self>,
        instance_id: String,
        delete_files: bool,
        record: PendingInstanceDeletion,
        gate: OwnedMutexGuard<()>,
    ) -> Result<SettledInstanceDeletion, InstanceDeletionSettlementFailure> {
        let mut candidate = self.current();
        candidate.pending_deletions.retain(|pending| pending != &record);
        InstanceDeletionMarkerClearRetry {
            guard: InstanceDeletionDropGuard::armed(),
            store: Arc::clone(self),
            gate,
            instance_id,
            delete_files,
            candidate,
            write: InstanceDeletionMarkerWrite::Prepare { encoded: None },
        }
        .retry()
        .await
    }

    #[cfg(test)]
    pub(crate) async fn close(&self) -> Result<(), InstanceStoreError> {
        self.close_admitted(|| Ok(None::<()>)).await
    }

    pub(super) async fn close_admitted<Admit, Admission>(
        &self,
        admit: Admit,
    ) -> Result<(), InstanceStoreError>
    where
        Admit: FnOnce() -> Result<Option<Admission>, InstanceStoreError>,
    {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let close = InstanceRegistryCloseTransition::begin(Arc::clone(&self.closed), gate);
        let _instance_content_admission = self
            .close_instance_content_roots()
            .await
            .map_err(InstanceStoreError::Persistence)?;
        let instances_directory = self
            .instance_directory_issuer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .ok_or_else(closed_instance_registry_error)?;
        instances_directory
            .identity()
            .map_err(InstanceStoreError::Persistence)?;
        let _reconciliation_admission = if self.has_managed_artifact_reconciliation() {
            admit()?
        } else {
            None
        };
        let close = self.reconcile_obligations(close).await?;
        if !self.current().pending_deletions.is_empty() {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::WouldBlock,
                "instance registry close requires deletion recovery settlement",
            )));
        }
        self.persistence
            .owner
            .close()
            .await
            .map_err(instance_persistence_error)?;
        let retired = self
            .instance_directory_issuer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| std::process::abort());
        drop((instances_directory, retired));
        close.finish();
        Ok(())
    }

    pub(super) fn has_managed_artifact_reconciliation(&self) -> bool {
        let state = self.state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
        if !state.visible.pending_deletions.is_empty() {
            return true;
        }
        state
            .retry_candidate
            .as_ref()
            .is_some_and(|(_, candidate)| {
                !candidate.pending_deletions.is_empty()
                    || candidate.pending_deletions != state.visible.pending_deletions
                    || !same_managed_instance_identities(&state.visible, candidate)
            })
    }

    async fn reconcile_retry<Gate>(
        &self,
        gate: Gate,
    ) -> Result<Gate, InstanceStoreError>
    where
        Gate: Send + 'static,
    {
        let retained = self
            .state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(gate);
        };
        let ticket = self
            .persistence
            .writer
            .retry()
            .map_err(instance_persistence_error)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "instance registry retry revision diverged from retained candidate"
        );
        self.await_commit(
            PendingInstanceRegistryCommit {
                ticket,
                revision,
                candidate,
            },
            gate,
        )
        .await
    }

    async fn reconcile_obligations<Gate>(
        &self,
        gate: Gate,
    ) -> Result<Gate, InstanceStoreError>
    where
        Gate: Send + 'static,
    {
        self.reconcile_retry(gate).await
    }

    async fn commit<ResultValue>(
        &self,
        candidate: InstanceRegistrySnapshot,
        result: ResultValue,
        gate: OwnedMutexGuard<()>,
    ) -> Result<ResultValue, InstanceStoreError>
    where
        ResultValue: Send + 'static,
    {
        let (result, gate) = self.commit_holding_gate(candidate, result, gate).await?;
        drop(gate);
        Ok(result)
    }

    async fn commit_holding_gate<ResultValue, Gate>(
        &self,
        candidate: InstanceRegistrySnapshot,
        result: ResultValue,
        gate: Gate,
    ) -> Result<(ResultValue, Gate), InstanceStoreError>
    where
        ResultValue: Send + 'static,
        Gate: Send + 'static,
    {
        let (candidate, encoded) = encode_instance_registry(candidate).await?;
        let ticket = self
            .persistence
            .writer
            .accept(encoded, WriteUrgency::Immediate, Ok)
            .map_err(instance_persistence_error)?;
        let revision = ticket.revision().get();
        let gate = self
            .await_commit(
                PendingInstanceRegistryCommit {
                    ticket,
                    revision,
                    candidate,
                },
                gate,
            )
            .await?;
        Ok((result, gate))
    }

    async fn await_commit<Gate>(
        &self,
        commit: PendingInstanceRegistryCommit,
        gate: Gate,
    ) -> Result<Gate, InstanceStoreError>
    where
        Gate: Send + 'static,
    {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
                    state.visible = commit.candidate.clone();
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|(revision, _)| *revision == commit.revision)
                    {
                        state.retry_candidate = None;
                    }
                    Ok(())
                }
                Err(error) => {
                    if matches!(error, PersistenceError::Write { .. }) {
                        state
                            .lock()
                            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
                            .retry_candidate = Some((commit.revision, commit.candidate));
                    }
                    Err(instance_persistence_error(error))
                }
            };
            let _ = completed_tx.send((result, gate));
        });
        let (result, gate) = completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(io::Error::other(
                "instance registry commit observer stopped before reporting completion",
            ))
        })?;
        result?;
        Ok(gate)
    }

    #[cfg(test)]
    pub fn insert_for_test(
        &self,
        name: impl Into<String>,
        version_id: impl Into<String>,
    ) -> Result<Instance, InstanceStoreError> {
        let name = name.into();
        let version_id = version_id.into();
        let id = generate_instance_id();
        ensure_instance_layout_blocking(&self.paths, &id)?;
        let instance = new_instance(id, name, version_id, String::new(), String::new());
        let mut state = self.state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
        let mut candidate = state.visible.clone();
        candidate.instances.push(instance.clone());
        candidate.validate()?;
        state.visible = candidate;
        Ok(instance)
    }

    #[cfg(test)]
    pub fn replace_for_test(&self, instance: Instance) -> Result<Instance, InstanceStoreError> {
        let mut state = self.state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
        let mut candidate = state.visible.clone();
        let Some(index) = candidate
            .instances
            .iter()
            .position(|stored| stored.id == instance.id)
        else {
            return Err(instance_not_found_error());
        };
        candidate.instances[index] = instance.clone();
        candidate.validate()?;
        state.visible = candidate;
        Ok(instance)
    }

    #[cfg(test)]
    pub fn remove_for_test(&self, instance_id: &str) -> Result<(), InstanceStoreError> {
        self.require_no_instance_content_root(instance_id)
            .map_err(InstanceStoreError::Persistence)?;
        let mut state = self.state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
        let mut candidate = state.visible.clone();
        let before = candidate.instances.len();
        candidate
            .instances
            .retain(|instance| instance.id != instance_id);
        if candidate.instances.len() == before {
            return Err(instance_not_found_error());
        }
        if candidate.last_instance_id == instance_id {
            candidate.last_instance_id.clear();
        }
        candidate.validate()?;
        state.visible = candidate;
        Ok(())
    }
}

fn same_managed_instance_identities(
    left: &InstanceRegistrySnapshot,
    right: &InstanceRegistrySnapshot,
) -> bool {
    left.instances.len() == right.instances.len()
        && left.instances.iter().all(|left_instance| {
            right.instances.iter().any(|right_instance| {
                left_instance.id == right_instance.id
                    && left_instance.version_id == right_instance.version_id
                    && left_instance.created_at == right_instance.created_at
            })
        })
}

pub(crate) fn new_instance(
    id: String,
    name: String,
    version_id: String,
    icon: String,
    accent: String,
) -> Instance {
    let art_seed = derive_instance_art_seed(&id, &name, &version_id);
    Instance {
        id,
        name,
        version_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        last_played_at: String::new(),
        art_seed,
        max_memory_mb: 0,
        min_memory_mb: 0,
        java_path: String::new(),
        window_width: 0,
        window_height: 0,
        jvm_preset: String::new(),
        performance_mode: String::new(),
        extra_jvm_args: String::new(),
        auto_optimize: false,
        icon,
        accent,
        loader_key: String::new(),
        minecraft_version: String::new(),
    }
}

fn ensure_insertable(
    snapshot: &InstanceRegistrySnapshot,
    instance: &Instance,
) -> Result<(), InstanceStoreError> {
    if snapshot
        .instances
        .iter()
        .any(|stored| stored.id == instance.id || stored.name == instance.name)
        || snapshot
            .pending_deletions
            .iter()
            .any(|pending| pending.instance_id == instance.id)
    {
        return Err(instance_name_conflict_error());
    }
    Ok(())
}

fn available_create_name(snapshot: &InstanceRegistrySnapshot, requested: &str) -> String {
    if !snapshot
        .instances
        .iter()
        .any(|instance| instance.name == requested)
    {
        return requested.to_string();
    }
    for index in 1..=snapshot.instances.len().saturating_add(1) {
        let candidate = format!("{requested} ({index})");
        if !snapshot
            .instances
            .iter()
            .any(|instance| instance.name == candidate)
        {
            return candidate;
        }
    }
    unreachable!("bounded registry must leave an available create name")
}

fn duplicate_name(
    snapshot: &InstanceRegistrySnapshot,
    source_name: &str,
    requested_name: Option<String>,
) -> Result<String, InstanceStoreError> {
    if let Some(name) = requested_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    {
        if snapshot
            .instances
            .iter()
            .any(|instance| instance.name == name)
        {
            return Err(instance_name_conflict_error());
        }
        return Ok(name);
    }
    let base = format!("{source_name} copy");
    if !snapshot
        .instances
        .iter()
        .any(|instance| instance.name == base)
    {
        return Ok(base);
    }
    for index in 2.. {
        let candidate = format!("{base} {index}");
        if !snapshot
            .instances
            .iter()
            .any(|instance| instance.name == candidate)
        {
            return Ok(candidate);
        }
    }
    unreachable!("bounded registry must leave an available duplicate name")
}

async fn prepare_new_instance_layout(
    paths: AppPaths,
    instance_id: String,
    library_dir: Option<PathBuf>,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        if !is_canonical_instance_id(&instance_id) {
            return Err(InstanceStoreError::Validation("instance id is invalid"));
        }
        ensure_instances_root(&paths)?;
        let game_dir = paths.instances_dir().join(&instance_id);
        std::fs::create_dir(&game_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                instance_name_conflict_error()
            } else {
                InstanceStoreError::Persistence(error)
            }
        })?;
        let prepared = (|| {
            ensure_instance_layout_blocking(&paths, &instance_id)?;
            if let Some(library_dir) = library_dir.as_deref() {
                seed_new_instance_files(library_dir, &game_dir)?;
            }
            Ok(())
        })();
        match prepared {
            Ok(()) => Ok(()),
            Err(error) => match std::fs::remove_dir_all(&game_dir) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(InstanceStoreError::Persistence(io::Error::other(
                    format!("{error}; failed to clean incomplete instance layout: {cleanup_error}"),
                ))),
            },
        }
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance layout task stopped: {error}"
        )))
    })?
}

fn ensure_instance_layout_blocking(
    paths: &AppPaths,
    instance_id: &str,
) -> Result<(), InstanceStoreError> {
    if !is_canonical_instance_id(instance_id) {
        return Err(InstanceStoreError::Validation("instance id is invalid"));
    }
    ensure_instances_root(paths)?;
    let game_dir = paths.instances_dir().join(instance_id);
    ensure_directory(&game_dir)?;
    for subdir in [
        "mods",
        "saves",
        "resourcepacks",
        "shaderpacks",
        "config",
        "screenshots",
        "logs",
    ] {
        ensure_directory(&game_dir.join(subdir))?;
    }
    Ok(())
}

fn seed_new_instance_files(source_dir: &Path, target_dir: &Path) -> Result<(), InstanceStoreError> {
    for file_name in ["options.txt", "servers.dat"] {
        seed_new_instance_file(source_dir, target_dir, file_name)
            .map_err(InstanceStoreError::Persistence)?;
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), InstanceStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance layout path is not a regular directory",
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(InstanceStoreError::Persistence)
        }
        Err(error) => Err(InstanceStoreError::Persistence(error)),
    }
}

fn ensure_instances_root(paths: &AppPaths) -> Result<(), InstanceStoreError> {
    match std::fs::symlink_metadata(paths.instances_dir()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(paths.instances_dir())
                .map_err(InstanceStoreError::Persistence)?;
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instances directory is not a regular directory",
            )));
        }
        Ok(_) => {}
        Err(error) => return Err(InstanceStoreError::Persistence(error)),
    }
    ensure_directory(paths.instances_dir())
}

async fn duplicate_instance_files(
    paths: AppPaths,
    source_id: String,
    target_id: String,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        ensure_instance_layout_blocking(&paths, &source_id)?;
        ensure_instances_root(&paths)?;
        let target_dir = paths.instances_dir().join(&target_id);
        std::fs::create_dir(&target_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                instance_name_conflict_error()
            } else {
                InstanceStoreError::Persistence(error)
            }
        })?;
        if let Err(error) = ensure_instance_layout_blocking(&paths, &target_id) {
            let _ = std::fs::remove_dir_all(&target_dir);
            return Err(error);
        }
        let source_dir = paths.instances_dir().join(source_id);
        let copied = (|| {
            for directory in ["mods", "saves", "resourcepacks", "shaderpacks", "config"] {
                copy_directory_contents(&source_dir.join(directory), &target_dir.join(directory))?;
            }
            for file_name in ["options.txt", "servers.dat"] {
                let source = source_dir.join(file_name);
                copy_regular_file_if_present(&source, &target_dir.join(file_name))?;
            }
            Ok(())
        })();
        if let Err(error) = copied {
            let _ = std::fs::remove_dir_all(&target_dir);
            return Err(error);
        }
        Ok(())
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance duplicate task stopped: {error}"
        )))
    })?
}

fn copy_directory_contents(source: &Path, target: &Path) -> Result<(), InstanceStoreError> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InstanceStoreError::Persistence(io::Error::new(
            io::ErrorKind::InvalidData,
            "instance resource directory is not a regular directory",
        )));
    }
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance resources cannot contain symbolic links",
            )));
        }
        if file_type.is_dir() {
            copy_directory_contents(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

async fn remove_uncommitted_instance_directory(
    paths: AppPaths,
    instance_id: String,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        let directory = paths.instances_dir().join(instance_id);
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(InstanceStoreError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "instance directory is not a regular directory",
                )))
            }
            Ok(_) => std::fs::remove_dir_all(directory).map_err(InstanceStoreError::Persistence),
            Err(error) => Err(InstanceStoreError::Persistence(error)),
        }
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "uncommitted instance cleanup task stopped: {error}"
        )))
    })?
}

async fn cleanup_failed_create(
    paths: &AppPaths,
    instance_id: &str,
    persistence_error: InstanceStoreError,
) -> InstanceStoreError {
    match remove_uncommitted_instance_directory(paths.clone(), instance_id.to_string()).await {
        Ok(()) => persistence_error,
        Err(cleanup_error) => InstanceStoreError::Persistence(io::Error::other(format!(
            "{persistence_error}; failed to clean uncommitted instance files: {cleanup_error}"
        ))),
    }
}

fn seed_new_instance_file(source_dir: &Path, target_dir: &Path, file_name: &str) -> io::Result<()> {
    let source = source_dir.join(file_name);
    match std::fs::symlink_metadata(&source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "new instance seed source is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) => return Err(error),
    }
    let target = target_dir.join(file_name);
    let mut input = std::fs::File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    io::copy(&mut input, &mut output)?;
    Ok(())
}

fn copy_regular_file_if_present(source: &Path, target: &Path) -> Result<(), InstanceStoreError> {
    match std::fs::symlink_metadata(source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance source file is not a regular file",
            )));
        }
        Ok(_) => {}
        Err(error) => return Err(InstanceStoreError::Persistence(error)),
    }
    if let Ok(metadata) = std::fs::symlink_metadata(target)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(InstanceStoreError::Persistence(io::Error::new(
            io::ErrorKind::InvalidData,
            "instance target file is not a regular file",
        )));
    }
    std::fs::copy(source, target)
        .map(|_| ())
        .map_err(InstanceStoreError::Persistence)
}

async fn encode_instance_registry(
    snapshot: InstanceRegistrySnapshot,
) -> Result<(InstanceRegistrySnapshot, Vec<u8>), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        let encoded = snapshot.encode()?;
        Ok((snapshot, encoded))
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance registry encoder stopped: {error}"
        )))
    })?
}

async fn encode_instance_registry_retained(
    snapshot: InstanceRegistrySnapshot,
) -> (InstanceRegistrySnapshot, Result<Vec<u8>, InstanceStoreError>) {
    tokio::task::spawn_blocking(move || {
        let encoded = snapshot.encode();
        (snapshot, encoded)
    })
    .await
    .unwrap_or_else(|_| std::process::abort())
}

fn admit_instance_source(
    source: &InstanceStore,
    directory: &AnchoredRecordDirectory,
) -> Result<(), InstanceStoreError> {
    if !source.mutation_allowed() {
        return Ok(());
    }
    let expected = match source.startup_source() {
        StartupFileProvenance::Accepted(bytes) => bytes,
        StartupFileProvenance::Missing => {
            return match directory.read(
                std::ffi::OsStr::new("instances.json"),
                INSTANCE_REGISTRY_MAX_BYTES,
            ) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(InstanceStoreError::Read(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "instance registry appeared after startup admission",
                ))),
                Err(error) => Err(InstanceStoreError::Read(error)),
            };
        }
        StartupFileProvenance::Synthetic => return Ok(()),
        StartupFileProvenance::Rejected => {
            return Err(InstanceStoreError::Read(io::Error::new(
                io::ErrorKind::InvalidData,
                "rejected instance registry source allowed mutation",
            )));
        }
    };
    let observation = match directory.read(
        std::ffi::OsStr::new("instances.json"),
        INSTANCE_REGISTRY_MAX_BYTES,
    ) {
        Ok(observation @ AnchoredRecordObservation::Bytes { .. }) => observation,
        Ok(AnchoredRecordObservation::Oversized { .. }) => {
            return Err(InstanceStoreError::TooLarge {
                max_bytes: INSTANCE_REGISTRY_MAX_BYTES,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(InstanceStoreError::Read(io::Error::new(
                io::ErrorKind::NotFound,
                "instance registry generation changed during startup",
            )));
        }
        Err(error) => return Err(InstanceStoreError::Read(error)),
    };
    let bytes = observation
        .bytes()
        .expect("bounded instance registry observation has bytes");
    if bytes != expected {
        return Err(InstanceStoreError::Read(io::Error::new(
            io::ErrorKind::InvalidData,
            "instance registry generation changed during startup",
        )));
    }
    let decoded = serde_json::from_slice::<InstanceRegistrySnapshot>(bytes)?;
    decoded.validate()?;
    if decoded != source.current() {
        return Err(InstanceStoreError::Read(io::Error::new(
            io::ErrorKind::InvalidData,
            "instance registry generation changed during startup",
        )));
    }
    observation
        .admit(INSTANCE_REGISTRY_MAX_BYTES)
        .map_err(instance_persistence_error)
}

fn instance_persistence_error(error: impl Into<io::Error>) -> InstanceStoreError {
    InstanceStoreError::Persistence(error.into())
}

fn retained_instance_deletion_error(error: &io::Error, message: &'static str) -> io::Error {
    io::Error::new(error.kind(), message)
}

fn invalid_instance_deletion_topology(message: &'static str) -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn pending_instance_deletion_recovery_error() -> InstanceDeletionPreparationFailure {
    InstanceDeletionPreparationFailure::Refused(InstanceStoreError::Persistence(io::Error::new(
        io::ErrorKind::WouldBlock,
        "instance deletion requires exact tombstone recovery",
    )))
}

pub(crate) fn instance_not_found_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::new(
        io::ErrorKind::NotFound,
        "instance not found",
    ))
}

pub(crate) fn instance_name_conflict_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "an instance with this name already exists",
    ))
}

fn closed_instance_registry_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(closed_instance_registry_io_error())
}

fn closed_instance_registry_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "instance registry persistence is closed",
    )
}

async fn settle_instance_content_retirement(
    retirement: Arc<ManagedTreeRetirement>,
    settlement: Arc<AsyncMutex<()>>,
) -> io::Result<()> {
    let settlement = settlement.lock_owned().await;
    retirement.wait_for_drain().await?;
    let permit = instance_content_settlement_gate()
        .acquire_owned()
        .await
        .map_err(|_| io::Error::other("instance content settlement gate was closed"))?;
    tokio::task::spawn_blocking(move || {
        let (_settlement, _permit) = (settlement, permit);
        retirement.settle_drained()
    })
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "instance content retirement task stopped: {error}"
        ))
    })?
}

fn instance_content_settlement_gate() -> Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| {
        Arc::new(Semaphore::new(INSTANCE_CONTENT_SETTLEMENT_CONCURRENCY))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use crate::state::managed_artifact_epoch::{
        ManagedArtifactMutationEpochCoordinator, ManagedArtifactMutationEpochUnavailable,
    };
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct RecordingBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
        attempted: Mutex<Vec<Vec<u8>>>,
        committed: Mutex<Vec<Vec<u8>>>,
        destinations: Mutex<Vec<PathBuf>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl RecordingBackend {
        fn new(failures: usize) -> Arc<Self> {
            Arc::new(Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(failures),
                started: Notify::new(),
                gate: Mutex::new(None),
                attempted: Mutex::new(Vec::new()),
                committed: Mutex::new(Vec::new()),
                destinations: Mutex::new(Vec::new()),
            })
        }

        fn gate_next(&self) -> Arc<WriteGate> {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            gate
        }

        async fn wait_for_attempt(&self, expected: usize) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) >= expected {
                    return;
                }
                started.await;
            }
        }

        fn committed_snapshots(&self) -> Vec<InstanceRegistrySnapshot> {
            self.committed
                .lock()
                .expect("committed registry lock")
                .iter()
                .map(|contents| {
                    serde_json::from_slice(contents).expect("decode committed instance registry")
                })
                .collect()
        }
    }

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.attempted
                .lock()
                .expect("attempted registry lock")
                .push(contents.to_vec());
            self.destinations
                .lock()
                .expect("registry destinations lock")
                .push(destination.test_path());
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("backend gate lock").take() {
                gate.wait();
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected instance registry write failure"));
            }
            self.committed
                .lock()
                .expect("committed registry lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }
    }

    #[tokio::test]
    async fn concurrent_mutations_derive_from_latest_committed_registry() {
        let (store, backend) = test_store("concurrent", InstanceRegistrySnapshot::default(), 0);
        let first = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .mutate(|snapshot| {
                        snapshot
                            .instances
                            .push(test_instance("0000000000000001", "First"));
                        Ok(())
                    })
                    .await
            })
        };
        let second = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .mutate(|snapshot| {
                        snapshot
                            .instances
                            .push(test_instance("0000000000000002", "Second"));
                        Ok(())
                    })
                    .await
            })
        };

        first
            .await
            .expect("first mutation task")
            .expect("first mutation");
        second
            .await
            .expect("second mutation task")
            .expect("second mutation");

        let current = store.current();
        assert_eq!(current.instances.len(), 2);
        assert!(
            current
                .instances
                .iter()
                .any(|instance| instance.name == "First")
        );
        assert!(
            current
                .instances
                .iter()
                .any(|instance| instance.name == "Second")
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn accepted_commit_survives_waiter_cancellation() {
        let (store, backend) = test_store("accepted-cancellation", snapshot_with_one(), 0);
        let gate = backend.gate_next();
        let first_store = store.clone();
        let first = tokio::spawn(async move {
            first_store
                .mutate(|snapshot| {
                    snapshot.instances[0].name = "Owned".to_string();
                    Ok(())
                })
                .await
        });
        backend.wait_for_attempt(1).await;
        first.abort();
        assert!(first.await.expect_err("cancel waiter").is_cancelled());
        gate.release();

        store
            .mutate(|snapshot| {
                snapshot.instances[0].performance_mode = "balanced".to_string();
                Ok(())
            })
            .await
            .expect("successor waits for owned commit");

        assert_eq!(store.current().instances[0].name, "Owned");
        assert_eq!(store.current().instances[0].performance_mode, "balanced");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_before_admission_drops_mutation_before_close() {
        let (store, backend) = test_store(
            "pre-admission-cancellation",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let gate = store
            .acquire_mutation()
            .await
            .expect("hold instance registry gate");
        let waiting_store = store.clone();
        let waiting = tokio::spawn(async move {
            waiting_store
                .mutate(|snapshot| {
                    snapshot
                        .instances
                        .push(test_instance("0000000000000001", "Must not commit"));
                    Ok(())
                })
                .await
        });
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(
            waiting
                .await
                .expect_err("cancel waiting mutation")
                .is_cancelled()
        );
        drop(gate);

        store
            .close()
            .await
            .expect("close after canceled pre-admission mutation");
        assert!(store.current().instances.is_empty());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_exact_bytes_commit_before_successor_derivation() {
        let (store, backend) = test_store("retry", snapshot_with_one(), 1);
        let first = store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Retained".to_string();
                Ok(())
            })
            .await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));
        assert_eq!(store.current().instances[0].name, "Original");

        store
            .mutate(|snapshot| {
                snapshot.instances[0].performance_mode = "balanced".to_string();
                Ok(())
            })
            .await
            .expect("successor reconciles retained bytes");

        let attempted = backend.attempted.lock().expect("attempted registry lock");
        assert_eq!(attempted.len(), 3);
        assert_eq!(attempted[0], attempted[1]);
        assert_ne!(attempted[1], attempted[2]);
        drop(attempted);

        let committed = backend.committed_snapshots();
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].instances[0].name, "Retained");
        assert!(committed[0].instances[0].performance_mode.is_empty());
        assert_eq!(committed[1].instances[0].name, "Retained");
        assert_eq!(committed[1].instances[0].performance_mode, "balanced");
    }

    #[tokio::test]
    async fn close_retries_retained_bytes_and_rejects_later_mutations() {
        let (store, backend) = test_store("close-retry", snapshot_with_one(), 1);
        let first = store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Retained for close".to_string();
                Ok(())
            })
            .await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));

        store
            .close()
            .await
            .expect("close retries retained registry");
        store.close().await.expect("close is idempotent");
        assert_eq!(store.current().instances[0].name, "Retained for close");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        let after_close = store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Must not commit".to_string();
                Ok(())
            })
            .await;
        assert!(matches!(
            after_close,
            Err(InstanceStoreError::Persistence(_))
        ));
        assert_eq!(store.current().instances[0].name, "Retained for close");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn validation_and_oversize_failures_accept_no_bytes() {
        let (store, backend) = test_store(
            "pre-acceptance-rejections",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let invalid = store
            .mutate(|snapshot| {
                snapshot.schema_version = u32::MAX;
                Ok(())
            })
            .await;
        assert!(matches!(invalid, Err(InstanceStoreError::Validation(_))));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);

        let oversized = store
            .mutate(|snapshot| {
                for index in 1..=100 {
                    let mut instance =
                        test_instance(&format!("{index:016x}"), &format!("Oversized {index}"));
                    instance.java_path = "j".repeat(4096);
                    instance.extra_jvm_args = "x".repeat(8192);
                    snapshot.instances.push(instance);
                }
                Ok(())
            })
            .await;
        assert!(matches!(
            oversized,
            Err(InstanceStoreError::TooLarge { .. })
        ));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(store.current(), InstanceRegistrySnapshot::default());

        let encoded_size = {
            let mut snapshot = InstanceRegistrySnapshot::default();
            for index in 1..=100 {
                let mut instance =
                    test_instance(&format!("{index:016x}"), &format!("Oversized {index}"));
                instance.java_path = "j".repeat(4096);
                instance.extra_jvm_args = "x".repeat(8192);
                snapshot.instances.push(instance);
            }
            serde_json::to_vec_pretty(&snapshot)
                .expect("encode oversized proof")
                .len() as u64
        };
        assert!(encoded_size > INSTANCE_REGISTRY_MAX_BYTES);
    }

    #[tokio::test]
    async fn invalid_create_prepares_no_directory_or_registry_bytes() {
        let (store, backend) = test_store("invalid-create", InstanceRegistrySnapshot::default(), 0);
        let instance = new_instance(
            "0000000000000001".to_string(),
            "x".repeat(129),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
        );
        let instance_path = store.game_dir(&instance.id);
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire invalid create mutation");

        let result = store.create_with_gate(instance, None, gate).await;

        assert!(matches!(result, Err(InstanceStoreError::Validation(_))));
        assert!(!instance_path.exists());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn failed_accepted_create_retains_directory_for_exact_retry() {
        let (store, backend) =
            test_store("create-exact-retry", InstanceRegistrySnapshot::default(), 1);
        let instance = test_instance("0000000000000001", "Retained create");
        let instance_path = store.game_dir(&instance.id);
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire create mutation");

        let first = store.create_with_gate(instance.clone(), None, gate).await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));
        assert!(instance_path.is_dir());
        assert!(store.current().instances.is_empty());

        store
            .mutate(|_| Ok(()))
            .await
            .expect("successor reconciles accepted create");
        assert_eq!(store.current().instances, vec![instance]);
        assert!(instance_path.is_dir());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        let attempted = backend.attempted.lock().expect("attempted registry lock");
        assert_eq!(attempted[0], attempted[1]);
        drop(attempted);
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn failed_create_retry_reacquires_epoch_until_identity_publication() {
        let (store, backend) =
            test_store("create-retry-epoch", InstanceRegistrySnapshot::default(), 1);
        let instance = test_instance("0000000000000001", "Epoch create retry");
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire create mutation");
        let first = store.create_with_gate(instance.clone(), None, gate).await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));
        assert!(store.current().instances.is_empty());

        let epoch = ManagedArtifactMutationEpochCoordinator::default();
        assert!(epoch.capture().is_ok(), "failed attempt releases its epoch");
        let write_gate = backend.gate_next();
        let owner_epoch = epoch.clone();
        let owner_store = store.clone();
        let close = tokio::spawn(async move {
            owner_store
                .close_admitted(move || {
                    owner_epoch.admit().map(Some).map_err(|error| {
                        InstanceStoreError::Persistence(std::io::Error::other(error.to_string()))
                    })
                })
                .await
        });
        backend.wait_for_attempt(2).await;

        assert!(matches!(
            epoch.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        ));
        assert!(store.current().instances.is_empty());
        write_gate.release();
        close
            .await
            .expect("close retry owner")
            .expect("close retries failed create");

        assert!(epoch.capture().is_ok());
        assert_eq!(store.current().instances, vec![instance]);
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn accepted_keep_files_delete_retries_exact_revision_before_publication() {
        let snapshot = snapshot_with_one();
        let instance = snapshot.instances[0].clone();
        let (store, backend) = test_store("keep-files-retry-epoch", snapshot, 1);
        let preserved = store.game_dir(&instance.id);
        std::fs::write(&preserved, b"preserved non-directory binding")
            .expect("seed keep-files binding");
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire delete mutation");
        let prepared = store
            .prepare_delete_with_gate(instance.id.clone(), false, gate)
            .await
            .unwrap_or_else(|_| panic!("prepare keep-files deletion"));
        let retry = match prepared.persist().await {
            Err(InstanceDeletionPersistenceFailure::Retryable { retry, .. }) => retry,
            _ => panic!("accepted deletion must retain its exact retry"),
        };
        assert_eq!(store.current().instances, vec![instance.clone()]);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), store.acquire_mutation())
                .await
                .is_err(),
            "retry carrier must retain the mutation gate"
        );

        let committed = retry
            .retry()
            .await
            .unwrap_or_else(|_| panic!("retry keep-files deletion"));
        assert!(store.current().instances.is_empty());
        let settled = committed
            .settle_files()
            .await
            .unwrap_or_else(|_| panic!("settle keep-files deletion"));
        assert!(!settled.deleted_files());
        drop(settled);
        assert_eq!(
            std::fs::read(&preserved).expect("read preserved keep-files binding"),
            b"preserved non-directory binding"
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        cleanup_test_store(store);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_rejects_symlinked_seed_file_and_cleans_uncommitted_directory() {
        use std::os::unix::fs::symlink;

        let (store, backend) = test_store(
            "create-symlink-source",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let seed_dir = store.paths().instances_dir().join("library-source");
        std::fs::create_dir_all(&seed_dir).expect("create seed source");
        let outside = store.paths().instances_dir().join("outside-options.txt");
        std::fs::write(&outside, b"outside").expect("write outside source");
        symlink(&outside, seed_dir.join("options.txt")).expect("symlink seed options");
        let instance = test_instance("0000000000000001", "Symlink source");
        let instance_path = store.game_dir(&instance.id);
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire create mutation");

        let result = store.create_with_gate(instance, Some(seed_dir), gate).await;

        assert!(matches!(result, Err(InstanceStoreError::Persistence(_))));
        assert!(!instance_path.exists());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn delete_files_parks_then_removes_exact_tree_and_clears_marker() {
        let snapshot = snapshot_with_one();
        let instance = snapshot.instances[0].clone();
        let instance_id = instance.id.clone();
        let pending = PendingInstanceDeletion::new(instance.id.clone(), instance.created_at)
            .expect("derive pending deletion");
        let (store, backend) = test_store("pending-deletion", snapshot, 0);
        std::fs::create_dir_all(store.paths().instances_dir()).expect("create instances root");
        let instance_path = store.game_dir(&instance_id);
        std::fs::create_dir(&instance_path).expect("create instance directory");
        std::fs::write(instance_path.join("owned.txt"), b"owned").expect("seed instance file");
        let tombstone_path = store
            .paths()
            .instances_dir()
            .join(&pending.tombstone_name);

        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire deletion mutation");
        let prepared = store
            .prepare_delete_with_gate(instance_id.clone(), true, gate)
            .await
            .unwrap_or_else(|_| panic!("prepare delete-files deletion"));
        assert!(!instance_path.exists());
        assert!(tombstone_path.is_dir());
        let committed = prepared
            .persist()
            .await
            .unwrap_or_else(|_| panic!("persist delete-files deletion"));
        assert!(store.current().instances.is_empty());
        assert_eq!(store.current().pending_deletions, vec![pending.clone()]);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        backend.failures.store(1, Ordering::SeqCst);
        let marker_retry = match committed.settle_files().await {
            Err(InstanceDeletionSettlementFailure::Marker { retry, .. }) => retry,
            _ => panic!("failed marker clear must retain its exact retry"),
        };
        assert!(!tombstone_path.exists());
        assert_eq!(store.current().pending_deletions, vec![pending.clone()]);
        let settled = marker_retry
            .retry()
            .await
            .unwrap_or_else(|_| panic!("retry deletion marker clear"));
        assert!(settled.deleted_files());
        drop(settled);
        assert!(!instance_path.exists());
        assert!(!tombstone_path.exists());
        assert!(store.current().pending_deletions.is_empty());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        let committed = backend.committed_snapshots();
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].pending_deletions, vec![pending]);
        assert!(committed[1].pending_deletions.is_empty());
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn startup_restores_a_live_instance_from_its_exact_tombstone() {
        let snapshot = snapshot_with_one();
        let instance = snapshot.instances[0].clone();
        let pending = PendingInstanceDeletion::new(instance.id.clone(), instance.created_at)
            .expect("derive live tombstone");
        let (store, _) = test_store("startup-live-tombstone", snapshot, 0);
        let canonical = store.game_dir(&instance.id);
        let tombstone = store
            .paths()
            .instances_dir()
            .join(&pending.tombstone_name);
        std::fs::create_dir(&canonical).expect("create live canonical directory");
        std::fs::rename(&canonical, &tombstone).expect("simulate crash after live park");

        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire startup recovery mutation");
        let recovery = store
            .prepare_startup_deletion_recovery_with_gate(gate)
            .await
            .expect("classify live tombstone")
            .unwrap_or_else(|| panic!("live tombstone requires recovery"));
        let retry = match recovery {
            InstanceDeletionStartupRecovery::RestoreLive(retry) => retry,
            InstanceDeletionStartupRecovery::CompletePending(_) => {
                panic!("live tombstone cannot complete a pending deletion")
            }
        };
        let settlement = retry
            .retry()
            .await
            .unwrap_or_else(|_| panic!("restore live tombstone"));
        match settlement {
            InstanceDeletionFilesystemSettlement::Aborted(aborted) => drop(aborted),
            InstanceDeletionFilesystemSettlement::Settled(_) => {
                panic!("live restoration cannot settle a deletion")
            }
        }
        assert!(canonical.is_dir());
        assert!(!tombstone.exists());
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn startup_completes_pending_tombstone_and_absent_tree_markers() {
        for (name, seed_tombstone) in [
            ("startup-pending-tombstone", true),
            ("startup-pending-absent", false),
        ] {
            let instance = test_instance("0000000000000001", "Pending");
            let pending = PendingInstanceDeletion::new(
                instance.id.clone(),
                instance.created_at.clone(),
            )
            .expect("derive pending tombstone");
            let snapshot = InstanceRegistrySnapshot::new(
                Vec::new(),
                String::new(),
                vec![pending.clone()],
            )
            .expect("pending-only snapshot");
            let (store, backend) = test_store(name, snapshot, 0);
            let tombstone = store
                .paths()
                .instances_dir()
                .join(&pending.tombstone_name);
            if seed_tombstone {
                std::fs::create_dir(&tombstone).expect("seed pending tombstone");
                std::fs::write(tombstone.join("owned.txt"), b"owned")
                    .expect("seed pending tombstone content");
            }

            let gate = store
                .acquire_mutation()
                .await
                .expect("acquire pending startup recovery");
            let recovery = store
                .prepare_startup_deletion_recovery_with_gate(gate)
                .await
                .expect("classify pending startup recovery")
                .unwrap_or_else(|| panic!("pending marker requires recovery"));
            let committed = match recovery {
                InstanceDeletionStartupRecovery::CompletePending(committed) => committed,
                InstanceDeletionStartupRecovery::RestoreLive(_) => {
                    panic!("pending marker cannot restore a live instance")
                }
            };
            let settled = committed
                .settle_files()
                .await
                .unwrap_or_else(|_| panic!("settle pending startup deletion"));
            assert!(settled.deleted_files());
            drop(settled);
            assert!(!tombstone.exists());
            assert!(store.current().pending_deletions.is_empty());
            assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
            cleanup_test_store(store);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_rejects_alias_wrong_kind_unrecognized_and_multiple_tombstones() {
        let alias_instance = test_instance("abcdefabcdefabcd", "Alias");
        let alias_pending = PendingInstanceDeletion::new(
            alias_instance.id.clone(),
            alias_instance.created_at.clone(),
        )
        .expect("derive alias tombstone");
        let alias_snapshot = InstanceRegistrySnapshot::new(
            vec![alias_instance],
            String::new(),
            Vec::new(),
        )
        .expect("alias snapshot");
        let (alias_store, _) = test_store("startup-alias", alias_snapshot, 0);
        std::fs::create_dir(
            alias_store
                .paths()
                .instances_dir()
                .join(alias_pending.tombstone_name.to_ascii_uppercase()),
        )
        .expect("seed portable tombstone alias");
        let gate = alias_store.acquire_mutation().await.expect("alias gate");
        assert!(matches!(
            alias_store
                .prepare_startup_deletion_recovery_with_gate(gate)
                .await,
            Err(InstanceStoreError::Persistence(_))
        ));
        cleanup_test_store(alias_store);

        let wrong_snapshot = snapshot_with_one();
        let wrong_id = wrong_snapshot.instances[0].id.clone();
        let (wrong_store, _) = test_store("startup-wrong-kind", wrong_snapshot, 0);
        std::fs::write(wrong_store.game_dir(&wrong_id), b"not a directory")
            .expect("seed wrong-kind canonical binding");
        let gate = wrong_store.acquire_mutation().await.expect("wrong-kind gate");
        assert!(matches!(
            wrong_store
                .prepare_startup_deletion_recovery_with_gate(gate)
                .await,
            Err(InstanceStoreError::Persistence(_))
        ));
        cleanup_test_store(wrong_store);

        let unrecognized_snapshot = snapshot_with_one();
        let (unrecognized_store, _) =
            test_store("startup-unrecognized-tombstone", unrecognized_snapshot, 0);
        std::fs::create_dir(
            unrecognized_store
                .paths()
                .instances_dir()
                .join(".axial-instance-tombstone-v1-unrecognized"),
        )
        .expect("seed unrecognized tombstone");
        let gate = unrecognized_store
            .acquire_mutation()
            .await
            .expect("unrecognized gate");
        assert!(matches!(
            unrecognized_store
                .prepare_startup_deletion_recovery_with_gate(gate)
                .await,
            Err(InstanceStoreError::Persistence(_))
        ));
        cleanup_test_store(unrecognized_store);

        let first = test_instance("0000000000000001", "First");
        let second = test_instance("0000000000000002", "Second");
        let first_pending = PendingInstanceDeletion::new(
            first.id.clone(),
            first.created_at.clone(),
        )
        .expect("derive first tombstone");
        let second_pending = PendingInstanceDeletion::new(
            second.id.clone(),
            second.created_at.clone(),
        )
        .expect("derive second tombstone");
        let multiple_snapshot = InstanceRegistrySnapshot::new(
            vec![first, second],
            String::new(),
            Vec::new(),
        )
        .expect("multiple live snapshot");
        let (multiple_store, _) = test_store("startup-multiple-tombstones", multiple_snapshot, 0);
        for pending in [first_pending, second_pending] {
            std::fs::create_dir(
                multiple_store
                    .paths()
                    .instances_dir()
                    .join(pending.tombstone_name),
            )
            .expect("seed recognized tombstone");
        }
        let gate = multiple_store
            .acquire_mutation()
            .await
            .expect("multiple tombstone gate");
        assert!(matches!(
            multiple_store
                .prepare_startup_deletion_recovery_with_gate(gate)
                .await,
            Err(InstanceStoreError::Persistence(_))
        ));
        cleanup_test_store(multiple_store);
    }

    #[test]
    fn topology_proof_accepts_more_than_legacy_orphan_threshold() {
        let snapshot = snapshot_with_one();
        let record = PendingInstanceDeletion::new(
            snapshot.instances[0].id.clone(),
            snapshot.instances[0].created_at.clone(),
        )
        .expect("derive topology target");
        let (store, _) = test_store("topology-preserved-orphans", snapshot.clone(), 0);
        for index in 0..3_100 {
            std::fs::create_dir(
                store
                    .paths()
                    .instances_dir()
                    .join(format!("preserved-orphan-{index:04}")),
            )
            .expect("seed preserved orphan");
        }
        let instances = store
            .instance_directory_issuer
            .lock()
            .expect("instance directory issuer lock")
            .as_ref()
            .cloned()
            .expect("instance directory authority");
        let proof = prove_instance_directory_topology(instances, &snapshot)
            .expect("complete orphan topology proof");
        let topology = proof.classify(&record).expect("classify absent live tree");
        assert!(topology.canonical.is_none());
        assert!(topology.tombstone.is_none());
        cleanup_test_store(store);
    }

    #[test]
    fn deletion_transaction_carriers_are_send_and_static() {
        fn assert_send_static<T: Send + 'static>() {}

        assert_send_static::<PreparedInstanceDeletion>();
        assert_send_static::<InstanceDeletionPreparationRetry>();
        assert_send_static::<InstanceDeletionPreparationFailure>();
        assert_send_static::<InstanceDeletionPersistenceRetry>();
        assert_send_static::<InstanceDeletionPersistenceFailure>();
        assert_send_static::<CommittedInstanceDeletion>();
        assert_send_static::<InstanceDeletionSettlementRetry>();
        assert_send_static::<InstanceDeletionMarkerClearRetry>();
        assert_send_static::<InstanceDeletionStartupRecovery>();
        assert_send_static::<InstanceDeletionSettlementFailure>();
    }

    #[tokio::test]
    async fn claim_owns_launcher_managed_registry_target_and_destination() {
        let paths = test_paths("ownership");
        let root_session = crate::state::test_root_session(&paths);
        let source = InstanceStore::from_snapshot(
            paths.clone(),
            root_session,
            InstanceRegistrySnapshot::default(),
        )
        .expect("instance source");
        let backend = RecordingBackend::new(0);
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = AppInstanceStore::claim_with_coordinator(&source, coordinator.clone())
            .expect("claim instance registry");
        assert!(matches!(
            AppInstanceStore::claim_with_coordinator(&source, coordinator),
            Err(InstanceStoreError::Persistence(_))
        ));

        store
            .mutate(|snapshot| {
                snapshot
                    .instances
                    .push(test_instance("0000000000000001", "Owned"));
                Ok(())
            })
            .await
            .expect("persist owned registry");

        assert_eq!(
            *backend
                .destinations
                .lock()
                .expect("registry destinations lock"),
            vec![paths.instances_file()]
        );
    }

    #[tokio::test]
    async fn managed_game_directory_retires_its_clean_operation_scoped_root() {
        let (store, _backend) = test_store(
            "managed-directory-owner",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Managed directory".to_string(), "1.21.1".to_string())
            .expect("insert instance");
        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("acquire content admission");
        let (operation, game) = store
            .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
            .expect("open managed game directory");
        drop(
            game.open_or_create_child("owned-child")
                .expect("create child through first request handle"),
        );
        drop((game, operation));
        store.release_managed_game_directory(&instance.id, lifecycle.incarnation());
        store
            .wait_for_instance_content_root_release(&instance.id)
            .await;
        assert!(
            store
                .instance_content_roots
                .lock()
                .expect("instance content roots")
                .is_empty(),
            "a clean request must not consume a persistent EffectOwner slot"
        );

        let (reopened_operation, reopened) = store
            .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
            .expect("mint a new operation-scoped request root");
        assert!(
            reopened
                .open_child("owned-child")
                .expect("inspect child through second handle")
                .is_some()
        );
        drop((reopened, reopened_operation));
        store.release_managed_game_directory(&instance.id, lifecycle.incarnation());
        store
            .wait_for_instance_content_root_release(&instance.id)
            .await;
        cleanup_test_store(store);
    }

    #[test]
    fn managed_instance_content_root_capacity_is_bounded() {
        let (store, _backend) = test_store(
            "managed-directory-capacity",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let reservations = (0..INSTANCE_CONTENT_ROOT_LIMIT)
            .map(|_| {
                store
                    .reserve_instance_content_root()
                    .expect("reserve bounded content root")
            })
            .collect::<Vec<_>>();
        assert!(store.reserve_instance_content_root().is_err_and(|error| {
            error.kind() == io::ErrorKind::WouldBlock
        }));
        assert_eq!(
            store.instance_content_root_count.load(Ordering::Acquire),
            INSTANCE_CONTENT_ROOT_LIMIT,
        );
        drop(reservations);
        assert_eq!(store.instance_content_root_count.load(Ordering::Acquire), 0);
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn managed_game_directory_refuses_a_stale_registered_generation() {
        let (store, _backend) = test_store(
            "managed-directory-registry-race",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Registry race".to_string(), "1.21.1".to_string())
            .expect("insert instance");
        store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Changed generation".to_string();
                Ok(())
            })
            .await
            .expect("replace registered generation");
        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("acquire content admission");
        assert!(
            store
                .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
                .is_err_and(|error| { error.kind() == io::ErrorKind::NotFound })
        );
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn managed_game_directory_preserves_a_lexical_replacement_after_binding_loss() {
        let (store, _backend) = test_store(
            "managed-directory-binding-loss",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Binding loss", "1.21.1")
            .expect("insert instance");
        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("acquire content admission");
        let (operation, directory) = store
            .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
            .expect("activate content root");
        let game_dir = store.game_dir(&instance.id);
        let displaced = store
            .paths()
            .instances_dir()
            .join(format!("{}-displaced", instance.id));
        std::fs::rename(&game_dir, &displaced).expect("displace bound instance directory");
        std::fs::create_dir(&game_dir).expect("create lexical replacement");
        std::fs::write(game_dir.join("replacement-marker"), b"replacement")
            .expect("write replacement marker");

        assert!(directory.open_or_create_child("must-not-appear").is_err());
        drop((directory, operation));
        assert!(
            store
                .retire_managed_game_directory(&instance.id, lifecycle.incarnation())
                .await
                .is_err(),
            "retirement must refuse settlement after the bound directory is displaced",
        );
        assert_eq!(
            std::fs::read(game_dir.join("replacement-marker"))
                .expect("replacement remains readable"),
            b"replacement",
        );
        assert!(!game_dir.join("must-not-appear").exists());

        drop((admission, lifecycle));
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn successful_close_retires_managed_directory_admission() {
        let (store, _backend) = test_store(
            "managed-directory-close",
            InstanceRegistrySnapshot::default(),
            0,
        );
        store
            .insert_for_test("Closing owner".to_string(), "1.21.1".to_string())
            .expect("insert instance");

        store.close().await.expect("close instance registry");

        assert!(
            store
                .acquire_instance_content_admission()
                .await
                .is_err_and(|error| { error.kind() == io::ErrorKind::AlreadyExists })
        );
        assert!(
            store
                .instance_directory_issuer
                .lock()
                .expect("instance directory issuer lock")
                .is_none()
        );
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn canceled_owner_close_restores_managed_directory_admission() {
        let (store, backend) = test_store(
            "managed-directory-canceled-close",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Canceled close".to_string(), "1.21.1".to_string())
            .expect("insert instance");
        let write_gate = backend.gate_next();
        let _accepted = store
            .persistence
            .writer
            .accept_encoded(b"owned close fence".to_vec(), WriteUrgency::Immediate)
            .expect("accept persistence write before close");
        backend.wait_for_attempt(1).await;

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.closed.load(Ordering::Acquire) || close.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close reaches the blocked persistence owner");
        close.abort();
        assert!(close.await.expect_err("cancel close waiter").is_cancelled());

        assert!(!store.closed.load(Ordering::Acquire));
        assert!(
            store
                .instance_directory_issuer
                .lock()
                .expect("instance directory issuer lock")
                .is_some()
        );
        write_gate.release();
        store.persistence.writer.wait_until_idle().await;

        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("content admission reopens after cancellation");
        let (operation, directory) = store
            .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
            .expect("managed directory admission reopens after cancellation");
        drop((directory, operation));
        store.release_managed_game_directory(&instance.id, lifecycle.incarnation());
        drop(admission);
        store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Reopened".to_string();
                Ok(())
            })
            .await
            .expect("mutation admission reopens after cancellation");

        store.close().await.expect("retry close succeeds");
        assert!(store.closed.load(Ordering::Acquire));
        assert!(
            store
                .instance_directory_issuer
                .lock()
                .expect("instance directory issuer lock")
                .is_none()
        );
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn canceled_close_waiting_for_content_context_reopens_admission() {
        let (store, _backend) = test_store(
            "managed-content-canceled-close",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Held content context", "1.21.1")
            .expect("insert instance");
        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("acquire content admission");
        let (operation, directory) = store
            .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
            .expect("activate held content root");

        let closing_store = store.clone();
        let mut close = tokio::spawn(async move { closing_store.close().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.closed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close seals instance admission");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut close)
                .await
                .is_err(),
            "close must wait for the admitted content context"
        );
        close.abort();
        assert!(close.await.expect_err("cancel close waiter").is_cancelled());
        assert!(!store.closed.load(Ordering::Acquire));

        drop((directory, operation));
        store.release_managed_game_directory(&instance.id, lifecycle.incarnation());
        drop(admission);
        let reopened = store
            .acquire_instance_content_admission()
            .await
            .expect("content admission reopens after canceled close");
        drop(reopened);

        store.close().await.expect("retry close succeeds");
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn close_waits_for_an_escaped_content_directory_pin() {
        let (store, _backend) = test_store(
            "managed-content-escaped-close-pin",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Escaped close pin", "1.21.1")
            .expect("insert instance");
        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("acquire content admission");
        let (operation, directory) = store
            .managed_game_directory(&instance, lifecycle.incarnation(), &admission)
            .expect("activate content root");
        let child = directory
            .open_or_create_child("escaped-child")
            .expect("derive escaped directory pin");
        drop((directory, operation, admission));

        let closing_store = Arc::clone(&store);
        let mut close = tokio::spawn(async move { closing_store.close().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.closed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close seals instance admission");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut close)
                .await
                .is_err(),
            "close must wait for a derived Core operation pin",
        );

        drop(child);
        close
            .await
            .expect("close task")
            .expect("close after escaped pin release");
        cleanup_test_store(store);
    }

    #[tokio::test]
    async fn deletion_retirement_waits_for_an_escaped_content_directory_pin() {
        let (store, _backend) = test_store(
            "managed-content-escaped-delete-pin",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let instance = store
            .insert_for_test("Escaped delete pin", "1.21.1")
            .expect("insert instance");
        let gates = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let lifecycle = gates.acquire(&instance.id).await;
        let incarnation = lifecycle.incarnation().clone();
        let admission = store
            .acquire_instance_content_admission()
            .await
            .expect("acquire content admission");
        let (operation, directory) = store
            .managed_game_directory(&instance, &incarnation, &admission)
            .expect("activate content root");
        let child = directory
            .open_or_create_child("escaped-child")
            .expect("derive escaped directory pin");
        drop((directory, operation));

        let retiring_store = Arc::clone(&store);
        let retained_instance_id = instance.id.clone();
        let mut retirement = tokio::spawn(async move {
            retiring_store
                .retire_managed_game_directory(&retained_instance_id, &incarnation)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut retirement)
                .await
                .is_err(),
            "delete retirement must wait for a derived Core operation pin",
        );

        drop(child);
        retirement
            .await
            .expect("retirement task")
            .expect("retirement after escaped pin release");
        drop((admission, lifecycle));
        cleanup_test_store(store);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_close_completes_after_blocked_close_is_canceled() {
        let (store, backend) = test_store(
            "managed-directory-queued-close",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let write_gate = backend.gate_next();
        let _accepted = store
            .persistence
            .writer
            .accept_encoded(b"queued close fence".to_vec(), WriteUrgency::Immediate)
            .expect("accept persistence write before close");
        backend.wait_for_attempt(1).await;

        let first_store = store.clone();
        let first_close = tokio::spawn(async move { first_store.close().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.closed.load(Ordering::Acquire) || first_close.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first close reaches the blocked persistence owner");

        let queued_store = store.clone();
        let queued_close = tokio::spawn(async move { queued_store.close().await });
        tokio::task::yield_now().await;
        assert!(!queued_close.is_finished());
        first_close.abort();
        assert!(
            first_close
                .await
                .expect_err("cancel first close waiter")
                .is_cancelled()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.closed.load(Ordering::Acquire) || queued_close.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued close starts instead of reporting false success");

        write_gate.release();
        queued_close
            .await
            .expect("join queued close")
            .expect("queued close succeeds");
        assert!(store.closed.load(Ordering::Acquire));
        assert!(
            store
                .instance_directory_issuer
                .lock()
                .expect("instance directory issuer lock")
                .is_none()
        );
        cleanup_test_store(store);
    }

    fn test_store(
        name: &str,
        snapshot: InstanceRegistrySnapshot,
        failures: usize,
    ) -> (Arc<AppInstanceStore>, Arc<RecordingBackend>) {
        let paths = test_paths(name);
        let root_session = crate::state::test_root_session(&paths);
        let source = InstanceStore::from_snapshot(paths, root_session, snapshot)
            .expect("instance source");
        let backend = RecordingBackend::new(failures);
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = AppInstanceStore::claim_with_coordinator(&source, coordinator)
            .expect("claim instance store");
        (Arc::new(store), backend)
    }

    #[test]
    fn startup_registry_provenance_requires_the_exact_loaded_generation() {
        let paths = test_paths("startup-provenance-accepted");
        std::fs::create_dir_all(paths.instances_file().parent().expect("registry parent"))
            .expect("create registry root");
        let bytes = snapshot_with_one().encode().expect("encode registry");
        std::fs::write(paths.instances_file(), &bytes).expect("seed registry");
        let root_session = crate::state::test_root_session(&paths);
        let source = InstanceStore::load_for_startup(paths.clone(), root_session.clone())
            .expect("load accepted registry")
            .store;
        let directory = AnchoredRecordDirectory::from_directory(
            root_session.clone(),
            root_session.root_directory().expect("root directory"),
        );
        admit_instance_source(&source, &directory).expect("unchanged registry is admitted");

        let replaced_paths = test_paths("startup-provenance-replaced");
        std::fs::create_dir_all(
            replaced_paths
                .instances_file()
                .parent()
                .expect("registry parent"),
        )
        .expect("create replaced root");
        std::fs::write(replaced_paths.instances_file(), &bytes).expect("seed replaced registry");
        let replaced_root = crate::state::test_root_session(&replaced_paths);
        let replaced_source =
            InstanceStore::load_for_startup(replaced_paths.clone(), replaced_root.clone())
                .expect("load replaced source")
                .store;
        let mut changed = bytes.clone();
        changed.push(b'\n');
        std::fs::write(replaced_paths.instances_file(), &changed)
            .expect("replace registry bytes");
        let replaced_directory = AnchoredRecordDirectory::from_directory(
            replaced_root.clone(),
            replaced_root.root_directory().expect("replaced root directory"),
        );
        assert!(admit_instance_source(&replaced_source, &replaced_directory).is_err());

        let removed_paths = test_paths("startup-provenance-removed");
        std::fs::create_dir_all(
            removed_paths
                .instances_file()
                .parent()
                .expect("registry parent"),
        )
        .expect("create removed root");
        std::fs::write(removed_paths.instances_file(), &bytes).expect("seed removed registry");
        let removed_root = crate::state::test_root_session(&removed_paths);
        let removed_source =
            InstanceStore::load_for_startup(removed_paths.clone(), removed_root.clone())
                .expect("load removed source")
                .store;
        std::fs::remove_file(removed_paths.instances_file()).expect("remove startup registry");
        let removed_directory = AnchoredRecordDirectory::from_directory(
            removed_root.clone(),
            removed_root.root_directory().expect("removed root directory"),
        );
        assert!(admit_instance_source(&removed_source, &removed_directory).is_err());

        let missing_paths = test_paths("startup-provenance-missing");
        std::fs::create_dir_all(
            missing_paths
                .instances_file()
                .parent()
                .expect("registry parent"),
        )
        .expect("create missing root");
        let missing_root = crate::state::test_root_session(&missing_paths);
        let missing_source =
            InstanceStore::load_for_startup(missing_paths.clone(), missing_root.clone())
                .expect("load missing source")
                .store;
        let missing_directory = AnchoredRecordDirectory::from_directory(
            missing_root.clone(),
            missing_root.root_directory().expect("missing root directory"),
        );
        admit_instance_source(&missing_source, &missing_directory)
            .expect("unchanged absence is admitted");
        std::fs::write(missing_paths.instances_file(), &bytes).expect("make registry appear");
        assert!(admit_instance_source(&missing_source, &missing_directory).is_err());

        drop((
            directory,
            replaced_directory,
            removed_directory,
            missing_directory,
        ));
        let _ = std::fs::remove_dir_all(paths.instances_file().parent().expect("registry root"));
        let _ = std::fs::remove_dir_all(
            replaced_paths
                .instances_file()
                .parent()
                .expect("registry root"),
        );
        let _ = std::fs::remove_dir_all(
            removed_paths
                .instances_file()
                .parent()
                .expect("registry root"),
        );
        let _ = std::fs::remove_dir_all(
            missing_paths
                .instances_file()
                .parent()
                .expect("registry root"),
        );
    }

    fn cleanup_test_store(store: Arc<AppInstanceStore>) {
        let instances_dir = store.paths().instances_dir().to_path_buf();
        let instances_file = store.paths().instances_file().to_path_buf();
        drop(store);
        let _ = std::fs::remove_dir_all(instances_dir);
        let _ = std::fs::remove_file(instances_file);
    }

    fn snapshot_with_one() -> InstanceRegistrySnapshot {
        InstanceRegistrySnapshot::new(
            vec![test_instance("0000000000000001", "Original")],
            String::new(),
            Vec::new(),
        )
        .expect("valid instance snapshot")
    }

    fn test_instance(id: &str, name: &str) -> Instance {
        new_instance(
            id.to_string(),
            name.to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
        )
    }

    fn test_paths(name: &str) -> AppPaths {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "axial-instance-registry-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
