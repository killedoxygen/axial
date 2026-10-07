//! Instance removal has a metadata commit point distinct from file cleanup.
//!
//! The journal is feature owned. In particular, keeping files never produces a
//! filesystem cleanup receipt, and a failed or interrupted deletion cannot turn
//! a caller supplied path into deletion authority.

use super::{
    create::{INITIAL_DIRECTORIES, NativeEffect, PendingAdmission},
    directory::{RegisteredInstance, Registry},
    model::{InstanceError, InstanceId, InstanceRecord},
};
use crate::storage::{
    Migration, StorageError,
    rusqlite::{self, OptionalExtension, params},
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Ordered after the instance registry by application composition.
pub const MIGRATION: Migration = Migration {
    id: "instance_deletions_v1",
    sql: "CREATE TABLE instance_deletions (
        operation_id TEXT PRIMARY KEY NOT NULL,
        instance_id TEXT NOT NULL,
        captured_revision INTEGER NOT NULL CHECK(captured_revision > 0),
        intent TEXT NOT NULL CHECK(intent IN ('keep_files', 'delete_files')),
        phase TEXT NOT NULL CHECK(phase IN ('prepared', 'committed', 'complete', 'aborted')),
        record_json TEXT NOT NULL,
        park_receipt TEXT,
        CHECK(intent = 'delete_files' OR park_receipt IS NULL),
        CHECK(intent = 'keep_files' OR park_receipt IS NOT NULL)
    );
    CREATE UNIQUE INDEX instance_deletion_unsettled
        ON instance_deletions(instance_id) WHERE phase IN ('prepared', 'committed');",
};

/// The caller must explicitly choose what happens to the instance payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DeleteIntent {
    KeepFiles,
    DeleteFiles,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeletionPhase {
    Prepared,
    Committed,
    Complete,
    Aborted,
}

impl DeletionPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Complete => "complete",
            Self::Aborted => "aborted",
        }
    }
}

/// Authoritative status distinguishes logical removal from physical cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DeletionStatus {
    PendingRestore,
    CleanupPending,
    Removed,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
pub struct DeletionSnapshot {
    pub operation_id: uuid::Uuid,
    pub instance_id: InstanceId,
    pub intent: DeleteIntent,
    pub status: DeletionStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum DeletionError {
    #[error("The deletion request does not match its accepted intent.")]
    IntentConflict,
    #[error("The instance changed. Refresh it before deleting it.")]
    Conflict,
    #[error("The instance files could not be verified. They were preserved.")]
    FilesPreserved,
    #[error("The deletion record could not be verified. Its files were preserved.")]
    InvalidRecord,
    #[error("The deletion was not found.")]
    NotFound,
    #[error("Instance deletion metadata could not be read or saved.")]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Instance(#[from] InstanceError),
}

pub type DeletionResult<T> = Result<T, DeletionError>;

impl super::create::InstanceService {
    /// Keep-files needs a logical target lease, but deliberately never opens or
    /// manufactures a cleanup capability for the payload.
    pub fn delete(
        &self,
        id: &InstanceId,
        intent: DeleteIntent,
        operation_id: uuid::Uuid,
    ) -> DeletionResult<crate::tasks::TaskHandle<DeletionResult<DeletionSnapshot>>> {
        if operation_id.is_nil() {
            return Err(DeletionError::IntentConflict);
        }
        let journal = DeletionJournal {
            registry: self.registry().clone(),
        };
        let existing = journal.get(operation_id)?;
        if existing
            .as_ref()
            .is_some_and(|record| record.instance.instance.id != *id || record.intent != intent)
        {
            return Err(DeletionError::IntentConflict);
        }
        let retained = if existing.is_some() {
            self.retained_admission(id)
        } else {
            None
        };
        let PendingAdmission { pin, lease } = match retained {
            Some(admission) => admission,
            None => PendingAdmission {
                pin: self
                    .directories
                    .library()
                    .admit()
                    .map_err(|_| InstanceError::LibraryUnavailable)?,
                lease: self
                    .directories
                    .exclusions()
                    .try_acquire([id.as_str()], [])
                    .map_err(|_| InstanceError::Busy)?,
            },
        };
        let captured = match &existing {
            Some(record) => record.instance.clone(),
            None => self.registry().get_live(id)?,
        };
        if existing.is_none()
            && crate::content::install::has_pending(self.registry().storage(), id)?
        {
            return Err(InstanceError::Busy.into());
        }
        if existing.is_none()
            && crate::performance::mutation::has_pending(self.registry().storage(), id)?
        {
            return Err(InstanceError::Busy.into());
        }
        // Explicit removal may abandon a setup intent, but never its active
        // admission or the content/Performance effects fenced above.
        let service = self.for_operation(id.clone());
        let existing_pending = existing.as_ref().is_some_and(|record| {
            matches!(
                record.phase,
                DeletionPhase::Prepared | DeletionPhase::Committed
            )
        });
        let rejected = (pin.clone(), lease.clone());
        let task = self
            .tasks
            .try_spawn((pin.clone(), lease.clone()), move |cancel| async move {
                tokio::task::spawn_blocking(move || {
                    if let Some(record) = existing {
                        let id = record.instance.instance.id.clone();
                        let result = service
                            .settle_effects()
                            .map_err(DeletionError::from)
                            .and_then(|()| {
                                service.settle_deletion(journal, record, pin.clone(), lease.clone())
                            });
                        if result.is_err() && existing_pending {
                            service.hold_admission(id, pin, lease);
                        }
                        return result;
                    }
                    if cancel.is_cancelled() {
                        return Err(InstanceError::Cancelled.into());
                    }
                    if intent == DeleteIntent::KeepFiles {
                        return journal
                            .keep_files(operation_id, &captured.instance.id, captured.revision)
                            .map(|record| record.snapshot());
                    }
                    let admitted = super::directory::InstanceDirectories::admit_record(
                        service.registry().clone(),
                        captured.clone(),
                        pin.clone(),
                        lease.clone(),
                    )?;
                    service.delete_files_admitted(journal, admitted, operation_id, &cancel, None)
                })
                .await
                .unwrap_or_else(|error| {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(InstanceError::Cancelled.into())
                })
            });
        if task.is_err() && existing_pending {
            self.hold_admission(id.clone(), rejected.0, rejected.1);
        }
        task.map_err(|_| InstanceError::Closed.into())
    }

    /// Queue removal transfers the setup's existing admission. Only the exact
    /// untouched creation qualifies for this automatic cleanup.
    pub(crate) fn delete_pristine_setup_admitted(
        &self,
        admitted: RegisteredInstance,
        operation_id: uuid::Uuid,
    ) -> DeletionResult<crate::tasks::TaskHandle<DeletionResult<Option<DeletionSnapshot>>>> {
        if operation_id.is_nil() {
            return Err(DeletionError::IntentConflict);
        }
        let id = admitted.record().instance.id.clone();
        let journal = DeletionJournal {
            registry: self.registry().clone(),
        };
        let existing = journal.get(operation_id)?;
        if existing.as_ref().is_some_and(|record| {
            record.instance.instance.id != id || record.intent != DeleteIntent::DeleteFiles
        }) {
            return Err(DeletionError::IntentConflict);
        }
        let admission = if existing.is_some() {
            self.retained_admission(&id)
        } else {
            None
        }
        .unwrap_or_else(|| PendingAdmission {
            pin: admitted.generation().clone(),
            lease: admitted.lease().clone(),
        });
        let pending = existing.as_ref().is_some_and(|record| {
            matches!(
                record.phase,
                DeletionPhase::Prepared | DeletionPhase::Committed
            )
        });
        let service = self.for_operation(id.clone());
        let rejected = admission.clone();
        let task = self
            .tasks
            .try_spawn(admission.clone(), move |cancel| async move {
                tokio::task::spawn_blocking(move || {
                    let PendingAdmission { pin, lease } = admission;
                    if let Some(record) = existing {
                        let result = service
                            .settle_effects()
                            .map_err(DeletionError::from)
                            .and_then(|()| {
                                service.settle_deletion(journal, record, pin.clone(), lease.clone())
                            });
                        if result.is_err() && pending {
                            service.hold_admission(
                                admitted.record().instance.id.clone(),
                                pin,
                                lease,
                            );
                        }
                        return result.map(Some);
                    }
                    if cancel.is_cancelled() {
                        return Err(InstanceError::Cancelled.into());
                    }
                    let Some(proof) = service.pristine_setup_proof(&admitted)? else {
                        return Ok(None);
                    };
                    service
                        .delete_files_admitted(
                            journal,
                            admitted,
                            operation_id,
                            &cancel,
                            Some(proof),
                        )
                        .map(Some)
                })
                .await
                .unwrap_or_else(|error| {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(InstanceError::Cancelled.into())
                })
            });
        if task.is_err() && pending {
            self.hold_admission(id, rejected.pin, rejected.lease);
        }
        task.map_err(|_| InstanceError::Closed.into())
    }

    fn pristine_setup_proof(
        &self,
        admitted: &RegisteredInstance,
    ) -> DeletionResult<Option<PristineSetupProof>> {
        use super::model::InstanceLifecycle;
        let id = &admitted.record().instance.id;
        if admitted.validate_current().is_err()
            || crate::content::install::has_pending(self.registry().storage(), id)?
            || crate::performance::mutation::has_pending(self.registry().storage(), id)?
        {
            return Ok(None);
        }
        let creation: Option<(String, String)> = self.registry().storage().read(|db| {
            db.query_row(
                "SELECT c.record_json,c.directory_receipt FROM instance_creations c
                 JOIN instance_setups s ON s.instance_id=c.instance_id
                 WHERE c.instance_id=?1 AND c.phase='complete' AND s.phase='pending'",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StorageError::from)
        })?;
        let Some((json, receipt)) = creation else {
            return Ok(None);
        };
        if json.len() > 4 * 1024 * 1024 {
            return Ok(None);
        }
        let Ok(mut original) = serde_json::from_str::<InstanceRecord>(&json) else {
            return Ok(None);
        };
        if original.lifecycle != InstanceLifecycle::Reserved
            || original.revision != 1
            || original.instance.revision != 1
            || original.directory_receipt.is_some()
        {
            return Ok(None);
        }
        // These are the only changes made by commit_reserved after publication.
        original.lifecycle = InstanceLifecycle::Live;
        original.revision += 1;
        original.instance.revision = original.revision;
        original.directory_receipt = Some(receipt);
        if &original != admitted.record() {
            return Ok(None);
        }
        Ok(PristineSetupProof::capture(admitted.game_directory()).ok())
    }

    fn delete_files_admitted(
        &self,
        journal: DeletionJournal,
        admitted: RegisteredInstance,
        operation_id: uuid::Uuid,
        cancel: &crate::tasks::CancellationToken,
        pristine: Option<PristineSetupProof>,
    ) -> DeletionResult<DeletionSnapshot> {
        admitted.validate_current()?;
        let captured = admitted.record();
        let pin = admitted.generation().clone();
        let lease = admitted.lease().clone();
        let parent = super::directory::instances_parent(&pin)?;
        let source_name = crate::files::PortableName::new_exact(&captured.directory_name)
            .map_err(|_| DeletionError::InvalidRecord)?;
        let park_name = crate::files::PortableName::new_exact(&format!("deleted-{operation_id}"))
            .map_err(|_| DeletionError::InvalidRecord)?;
        let plan = parent
            .plan_park(&source_name, &park_name)
            .map_err(|_| DeletionError::FilesPreserved)?;
        if !plan.receipt().matches_source_receipt(
            captured
                .directory_receipt
                .as_deref()
                .ok_or(DeletionError::InvalidRecord)?,
        ) {
            return Err(DeletionError::FilesPreserved);
        }
        if let Some(proof) = &pristine {
            proof
                .revalidate()
                .map_err(|_| DeletionError::FilesPreserved)?;
        }
        let mut record = journal.prepare_files(operation_id, captured, plan.receipt().encode())?;
        let result = (|| {
            if cancel.is_cancelled() {
                journal.restored(&mut record)?;
                return Ok(record.snapshot());
            }
            if pristine
                .as_ref()
                .is_some_and(|proof| proof.revalidate().is_err())
            {
                journal.restored(&mut record)?;
                return Err(DeletionError::FilesPreserved);
            }
            let (outcome, retained_pin) = plan.execute().into_parts();
            match outcome {
                axial_fs::DirectoryParkOutcome::Parked(parked) => {
                    if let Err(error) = journal.commit(&mut record) {
                        match parked.restore() {
                            axial_fs::DirectoryRestoreOutcome::Restored(_) => {
                                journal.restored(&mut record)?;
                            }
                            unresolved => self
                                .retain(NativeEffect::DirectoryRestore(unresolved, retained_pin)),
                        }
                        return Err(error);
                    }
                    self.remove_parked(parked, retained_pin)?;
                    journal.cleaned(&mut record)?;
                    Ok(record.snapshot())
                }
                axial_fs::DirectoryParkOutcome::NoEffect { .. } => {
                    journal.restored(&mut record)?;
                    Err(DeletionError::FilesPreserved)
                }
                unresolved => {
                    self.retain(NativeEffect::DirectoryPark(unresolved, retained_pin));
                    Err(InstanceError::SettlementRequired.into())
                }
            }
        })();
        if result.is_err()
            && matches!(
                record.phase,
                DeletionPhase::Prepared | DeletionPhase::Committed
            )
        {
            self.hold_admission(record.instance.instance.id.clone(), pin, lease);
        }
        result
    }

    pub fn deletion_status(&self, operation_id: uuid::Uuid) -> DeletionResult<DeletionSnapshot> {
        DeletionJournal {
            registry: self.registry().clone(),
        }
        .get(operation_id)?
        .map(|record| record.snapshot())
        .ok_or(DeletionError::NotFound)
    }

    pub fn pending_deletions(&self) -> DeletionResult<Vec<DeletionSnapshot>> {
        DeletionJournal {
            registry: self.registry().clone(),
        }
        .pending()
        .map(|records| {
            records
                .into_iter()
                .map(|record| record.snapshot())
                .collect()
        })
    }

    fn settle_deletion(
        &self,
        journal: DeletionJournal,
        mut record: DeletionRecord,
        pin: crate::library::GenerationPin,
        lease: crate::tasks::ExclusionLease,
    ) -> DeletionResult<DeletionSnapshot> {
        if matches!(
            record.phase,
            DeletionPhase::Complete | DeletionPhase::Aborted
        ) {
            return Ok(record.snapshot());
        }
        if record.instance.library_id != pin.library_id().to_string() {
            return Err(InstanceError::LibraryUnavailable.into());
        }
        let receipt = crate::files::DirectoryParkReceipt::decode(
            record
                .park_receipt
                .as_deref()
                .ok_or(DeletionError::InvalidRecord)?,
        )
        .map_err(|_| DeletionError::InvalidRecord)?;
        if receipt.source_name() != record.instance.directory_name
            || !receipt.matches_source_receipt(
                record
                    .instance
                    .directory_receipt
                    .as_deref()
                    .ok_or(DeletionError::InvalidRecord)?,
            )
        {
            return Err(DeletionError::InvalidRecord);
        }
        let parent = super::directory::instances_parent(&pin)?;
        let recovered = parent
            .recover_park(&receipt)
            .map_err(|_| DeletionError::FilesPreserved)?;
        let result = match recovered {
            crate::files::RecoveredDirectoryPark::Original(_)
                if record.phase == DeletionPhase::Prepared =>
            {
                journal.restored(&mut record)
            }
            crate::files::RecoveredDirectoryPark::Parked(retained) => {
                let (parked, pin) = retained.into_parts();
                if record.phase == DeletionPhase::Prepared {
                    match parked.restore() {
                        axial_fs::DirectoryRestoreOutcome::Restored(_) => {
                            journal.restored(&mut record)
                        }
                        unresolved => {
                            self.retain(NativeEffect::DirectoryRestore(unresolved, pin));
                            Err(InstanceError::SettlementRequired.into())
                        }
                    }
                } else {
                    self.remove_parked(parked, pin)
                        .map_err(DeletionError::from)
                        .and_then(|()| journal.cleaned(&mut record))
                }
            }
            crate::files::RecoveredDirectoryPark::Missing
                if record.phase == DeletionPhase::Committed =>
            {
                journal.cleaned(&mut record)
            }
            _ => Err(DeletionError::FilesPreserved),
        };
        if result.is_err() {
            self.hold_admission(record.instance.instance.id.clone(), pin, lease);
        }
        result.map(|()| record.snapshot())
    }
}

struct PristineSetupProof(Vec<(crate::files::ScopedDirectory, axial_fs::DirectoryRevision)>);

impl PristineSetupProof {
    fn capture(root: &crate::files::ScopedDirectory) -> std::io::Result<Self> {
        let root_revision = root.revision()?;
        let listing = root.entries(INITIAL_DIRECTORIES.len())?;
        if listing.state() != axial_fs::DirectoryListingState::Complete
            || listing.entries().len() != INITIAL_DIRECTORIES.len()
            || listing.entries().iter().any(|entry| {
                entry.kind() != axial_fs::EntryKind::Directory
                    || !entry
                        .utf8_name()
                        .is_some_and(|name| INITIAL_DIRECTORIES.contains(&name))
            })
        {
            return Err(std::io::Error::other("setup payload differs from creation"));
        }
        let mut directories = vec![(root.clone(), root_revision)];
        for name in INITIAL_DIRECTORIES {
            let directory = root.open_directory(
                &crate::files::PortableName::new_exact(name).expect("fixed portable name"),
            )?;
            let revision = directory.revision()?;
            let contents = directory.entries(1)?;
            if contents.state() != axial_fs::DirectoryListingState::Complete
                || !contents.entries().is_empty()
            {
                return Err(std::io::Error::other("setup directory contains user data"));
            }
            directories.push((directory, revision));
        }
        let proof = Self(directories);
        proof.revalidate()?;
        Ok(proof)
    }

    fn revalidate(&self) -> std::io::Result<()> {
        for (directory, revision) in &self.0 {
            directory.validate_revision(revision)?;
        }
        Ok(())
    }
}

struct DeletionRecord {
    operation_id: uuid::Uuid,
    instance: InstanceRecord,
    intent: DeleteIntent,
    phase: DeletionPhase,
    /// Opaque file-owner receipt. Parsing this string never creates authority.
    park_receipt: Option<String>,
}

impl DeletionRecord {
    fn snapshot(&self) -> DeletionSnapshot {
        DeletionSnapshot {
            operation_id: self.operation_id,
            instance_id: self.instance.instance.id.clone(),
            intent: self.intent,
            status: match self.phase {
                DeletionPhase::Prepared => DeletionStatus::PendingRestore,
                DeletionPhase::Committed => DeletionStatus::CleanupPending,
                DeletionPhase::Complete => DeletionStatus::Removed,
                DeletionPhase::Aborted => DeletionStatus::Aborted,
            },
        }
    }
}

#[derive(Clone)]
struct DeletionJournal {
    registry: Registry,
}

impl DeletionJournal {
    fn get(&self, operation_id: uuid::Uuid) -> DeletionResult<Option<DeletionRecord>> {
        self.registry
            .storage()
            .read(|connection| read_record(connection, operation_id))
    }

    fn pending(&self) -> DeletionResult<Vec<DeletionRecord>> {
        self.registry.storage().read(|connection| {
            let mut statement = connection.prepare(
                "SELECT operation_id FROM instance_deletions WHERE phase IN ('prepared', 'committed') ORDER BY operation_id"
            ).map_err(StorageError::from)?;
            let identities = statement.query_map([], |row| row.get::<_, String>(0))
                .map_err(StorageError::from)?
                .collect::<Result<Vec<_>, _>>().map_err(StorageError::from)?;
            identities.into_iter().map(|identity| {
                let id = uuid::Uuid::parse_str(&identity).map_err(|_| DeletionError::InvalidRecord)?;
                read_record(connection, id)?.ok_or(DeletionError::InvalidRecord)
            }).collect()
        })
    }

    /// Keep-files commits in one transaction and never accepts a file receipt.
    fn keep_files(
        &self,
        operation_id: uuid::Uuid,
        id: &InstanceId,
        revision: u64,
    ) -> DeletionResult<DeletionRecord> {
        self.registry.storage().transaction(|transaction| {
            let instance = self.registry.mark_deleting(transaction, id, revision)?;
            self.registry.remove(transaction, &instance)?;
            transaction
                .execute(
                    "DELETE FROM instance_setups WHERE instance_id=?1 AND phase='pending'",
                    [instance.instance.id.as_str()],
                )
                .map_err(StorageError::from)?;
            let record = DeletionRecord {
                operation_id,
                instance,
                intent: DeleteIntent::KeepFiles,
                phase: DeletionPhase::Complete,
                park_receipt: None,
            };
            insert_record(transaction, &record)?;
            Ok(record)
        })
    }

    /// Commit the exact recovery descriptor before the file owner can move data.
    fn prepare_files(
        &self,
        operation_id: uuid::Uuid,
        captured: &InstanceRecord,
        receipt: String,
    ) -> DeletionResult<DeletionRecord> {
        self.registry.storage().transaction(|transaction| {
            let instance = self.registry.mark_deleting(
                transaction,
                &captured.instance.id,
                captured.revision,
            )?;
            if instance.library_id != captured.library_id
                || instance.directory_name != captured.directory_name
                || instance.directory_receipt != captured.directory_receipt
            {
                return Err(DeletionError::Conflict);
            }
            let record = DeletionRecord {
                operation_id,
                instance,
                intent: DeleteIntent::DeleteFiles,
                phase: DeletionPhase::Prepared,
                park_receipt: Some(receipt),
            };
            insert_record(transaction, &record)?;
            Ok(record)
        })
    }

    fn commit(&self, record: &mut DeletionRecord) -> DeletionResult<()> {
        self.registry.storage().transaction(|transaction| {
            self.registry.remove(transaction, &record.instance)?;
            transaction
                .execute(
                    "DELETE FROM instance_setups WHERE instance_id=?1 AND phase='pending'",
                    [record.instance.instance.id.as_str()],
                )
                .map_err(StorageError::from)?;
            update_phase(transaction, record, DeletionPhase::Committed)
        })?;
        record.phase = DeletionPhase::Committed;
        Ok(())
    }

    fn restored(&self, record: &mut DeletionRecord) -> DeletionResult<()> {
        self.registry.storage().transaction(|transaction| {
            self.registry.restore_live(transaction, &record.instance)?;
            update_phase(transaction, record, DeletionPhase::Aborted)
        })?;
        record.phase = DeletionPhase::Aborted;
        Ok(())
    }

    fn cleaned(&self, record: &mut DeletionRecord) -> DeletionResult<()> {
        self.registry.storage().transaction(|transaction| {
            update_phase(transaction, record, DeletionPhase::Complete)
        })?;
        record.phase = DeletionPhase::Complete;
        Ok(())
    }
}

fn insert_record(
    transaction: &rusqlite::Transaction<'_>,
    record: &DeletionRecord,
) -> DeletionResult<()> {
    if record.operation_id.is_nil() {
        return Err(DeletionError::IntentConflict);
    }
    let serialized =
        serde_json::to_string(&record.instance).map_err(|_| DeletionError::InvalidRecord)?;
    transaction.execute(
        "INSERT INTO instance_deletions(operation_id, instance_id, captured_revision, intent, phase, record_json, park_receipt) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![record.operation_id.to_string(), record.instance.instance.id.as_str(), record.instance.revision,
            match record.intent { DeleteIntent::KeepFiles => "keep_files", DeleteIntent::DeleteFiles => "delete_files" },
            record.phase.as_str(), serialized, record.park_receipt],
    ).map_err(StorageError::from)?;
    Ok(())
}

fn update_phase(
    transaction: &rusqlite::Transaction<'_>,
    record: &DeletionRecord,
    next: DeletionPhase,
) -> DeletionResult<()> {
    let changed = transaction.execute(
        "UPDATE instance_deletions SET phase = ?1 WHERE operation_id = ?2 AND instance_id = ?3 AND captured_revision = ?4 AND phase = ?5",
        params![next.as_str(), record.operation_id.to_string(), record.instance.instance.id.as_str(), record.instance.revision, record.phase.as_str()],
    ).map_err(StorageError::from)?;
    if changed != 1 {
        return Err(DeletionError::Conflict);
    }
    Ok(())
}

fn read_record(
    connection: &rusqlite::Connection,
    id: uuid::Uuid,
) -> DeletionResult<Option<DeletionRecord>> {
    let raw = connection.query_row(
        "SELECT instance_id, captured_revision, intent, phase, record_json, park_receipt FROM instance_deletions WHERE operation_id = ?1",
        [id.to_string()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?)),
    ).optional().map_err(StorageError::from)?;
    let Some((instance_id, revision, intent, phase, json, receipt)) = raw else {
        return Ok(None);
    };
    if id.is_nil()
        || json.len() > 64 * 1024
        || receipt
            .as_ref()
            .is_some_and(|value| value.len() > 64 * 1024)
    {
        return Err(DeletionError::InvalidRecord);
    }
    let instance: InstanceRecord =
        serde_json::from_str(&json).map_err(|_| DeletionError::InvalidRecord)?;
    if instance.instance.id.as_str() != instance_id
        || instance.revision != revision
        || instance.lifecycle != super::model::InstanceLifecycle::Deleting
    {
        return Err(DeletionError::InvalidRecord);
    }
    let intent = match intent.as_str() {
        "keep_files" if receipt.is_none() => DeleteIntent::KeepFiles,
        "delete_files" if receipt.is_some() => DeleteIntent::DeleteFiles,
        _ => return Err(DeletionError::InvalidRecord),
    };
    let phase = match phase.as_str() {
        "prepared" => DeletionPhase::Prepared,
        "committed" => DeletionPhase::Committed,
        "complete" => DeletionPhase::Complete,
        "aborted" => DeletionPhase::Aborted,
        _ => return Err(DeletionError::InvalidRecord),
    };
    if intent == DeleteIntent::KeepFiles && phase != DeletionPhase::Complete {
        return Err(DeletionError::InvalidRecord);
    }
    Ok(Some(DeletionRecord {
        operation_id: id,
        instance,
        intent,
        phase,
        park_receipt: receipt,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::{
        create::{InstanceService, tests as creation},
        model::{Instance, InstanceLifecycle},
    };
    use super::*;
    use crate::{files::PortableName, storage::MetadataStore};
    use std::{path::PathBuf, sync::Arc};

    fn fixture() -> (DeletionJournal, InstanceRecord) {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                super::super::directory::MIGRATION,
                super::super::create::MIGRATION,
                MIGRATION,
            ])
            .unwrap();
        let registry = Registry::new(storage);
        let record = insert_instance(&registry);
        (DeletionJournal { registry }, record)
    }

    #[tokio::test]
    async fn rejected_deletion_retry_preserves_its_exact_admission_for_later_settlement() {
        use super::super::create::{CreateInstanceRequest, CreateTarget};
        use crate::files::PortableName;
        let (_root, mut service) = super::super::create::tests::fixture();
        let instance = service
            .create(
                CreateInstanceRequest {
                    name: "Retry deletion".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: "vanilla|1.21.4".into(),
                    version_id: "1.21.4".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "vanilla".into(),
                },
                service.creation_admission_for_tests().await.unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let admitted = service.directories.admit(&instance.id).unwrap();
        let journal = DeletionJournal {
            registry: service.registry().clone(),
        };
        let operation = uuid::Uuid::new_v4();
        let parent = super::super::directory::instances_parent(admitted.generation()).unwrap();
        let plan = parent
            .plan_park(
                &PortableName::new_exact(instance.id.as_str()).unwrap(),
                &PortableName::new_exact(&format!("deleted-{operation}")).unwrap(),
            )
            .unwrap();
        journal
            .prepare_files(operation, admitted.record(), plan.receipt().encode())
            .unwrap();
        service.hold_admission(
            instance.id.clone(),
            admitted.generation().clone(),
            admitted.lease().clone(),
        );
        drop(admitted);
        drop(plan);
        service
            .tasks
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(
            service.delete(&instance.id, DeleteIntent::DeleteFiles, operation),
            Err(DeletionError::Instance(InstanceError::Closed))
        ));
        assert!(service.has_unsettled_effects());
        assert!(
            service
                .directories
                .exclusions()
                .try_acquire([instance.id.as_str()], [])
                .is_err()
        );
        // Replacing only this test's closed task owner permits an explicit retry
        // using the original admission, not a fresh lease that would be Busy.
        service.tasks = crate::tasks::TaskOwner::new(16).unwrap();
        let recovered = service
            .delete(&instance.id, DeleteIntent::DeleteFiles, operation)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, DeletionStatus::Aborted);
        assert!(!service.has_unsettled_effects());
        service
            .directories
            .admit(&instance.id)
            .unwrap()
            .validate_current()
            .unwrap();
    }

    fn insert_instance(registry: &Registry) -> InstanceRecord {
        let instance = Instance {
            id: InstanceId::new(),
            name: "Deletion fixture".into(),
            version_id: "1.21.4".into(),
            created_at: "2026-09-08T00:00:00Z".into(),
            last_played_at: String::new(),
            art_seed: 1,
            settings: Default::default(),
            icon: String::new(),
            accent: String::new(),
            loader_key: "vanilla".into(),
            minecraft_version: "1.21.4".into(),
            revision: 1,
        };
        registry
            .storage()
            .transaction(|tx| -> Result<_, InstanceError> {
                let record = registry.reserve(tx, instance, "isolated-fixture")?;
                // This test exercises only metadata, never file-owner admission.
                registry.commit_reserved(tx, &record, "opaque-fixture-receipt")
            })
            .unwrap()
    }

    #[test]
    fn keep_files_commits_removal_without_any_cleanup_authority() {
        let (journal, instance) = fixture();
        journal.registry.select(&instance.instance.id).unwrap();
        let id = uuid::Uuid::new_v4();
        let deleted = journal
            .keep_files(id, &instance.instance.id, instance.revision)
            .unwrap();
        assert_eq!(deleted.snapshot().status, DeletionStatus::Removed);
        assert!(deleted.park_receipt.is_none());
        assert!(journal.registry.list().unwrap().is_empty());
        assert!(journal.registry.last_instance_id().unwrap().is_none());
        assert!(journal.pending().unwrap().is_empty());
        assert_eq!(
            journal.get(id).unwrap().unwrap().snapshot(),
            deleted.snapshot()
        );
    }

    #[test]
    fn journal_insert_failure_rolls_back_logical_removal_and_selection() {
        let (journal, instance) = fixture();
        journal.registry.select(&instance.instance.id).unwrap();
        assert!(
            journal
                .keep_files(uuid::Uuid::nil(), &instance.instance.id, instance.revision)
                .is_err()
        );
        assert_eq!(
            journal.registry.get_live(&instance.instance.id).unwrap(),
            instance
        );
        assert_eq!(
            journal.registry.last_instance_id().unwrap(),
            Some(instance.instance.id)
        );
    }

    #[test]
    fn cleanup_failure_cannot_undo_committed_logical_removal() {
        let (journal, instance) = fixture();
        let id = uuid::Uuid::new_v4();
        let mut record = journal
            .prepare_files(id, &instance, "file-owner-plan".into())
            .unwrap();
        assert!(matches!(
            journal.registry.get_live(&instance.instance.id),
            Err(InstanceError::Busy)
        ));
        journal.commit(&mut record).unwrap();
        assert_eq!(record.snapshot().status, DeletionStatus::CleanupPending);
        assert!(matches!(
            journal.registry.get_record(&instance.instance.id),
            Err(InstanceError::NotFound)
        ));
        assert!(journal.restored(&mut record).is_err());
        assert_eq!(
            journal.pending().unwrap()[0].snapshot().status,
            DeletionStatus::CleanupPending
        );
        journal.cleaned(&mut record).unwrap();
        assert_eq!(
            journal.get(id).unwrap().unwrap().snapshot().status,
            DeletionStatus::Removed
        );
    }

    #[test]
    fn precommit_restore_reopens_only_the_exact_deleting_record() {
        let (journal, instance) = fixture();
        let mut record = journal
            .prepare_files(uuid::Uuid::new_v4(), &instance, "file-owner-plan".into())
            .unwrap();
        journal.restored(&mut record).unwrap();
        let restored = journal.registry.get_live(&instance.instance.id).unwrap();
        assert_eq!(restored.instance.name, instance.instance.name);
        assert_eq!(restored.directory_receipt, instance.directory_receipt);
        assert_eq!(restored.lifecycle, InstanceLifecycle::Live);
        assert!(restored.revision > instance.revision);
        assert_eq!(record.snapshot().status, DeletionStatus::Aborted);
        assert!(journal.commit(&mut record).is_err());
    }

    #[test]
    fn durable_restart_preserves_precommit_and_postcommit_obligations() {
        let root = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let metadata = root.path().join("metadata.sqlite");
        let storage = Arc::new(MetadataStore::open(&metadata).unwrap());
        storage
            .migrate(&[
                super::super::directory::MIGRATION,
                super::super::create::MIGRATION,
                MIGRATION,
            ])
            .unwrap();
        let registry = Registry::new(storage);
        let instance = insert_instance(&registry);
        let journal = DeletionJournal { registry };
        let operation_id = uuid::Uuid::new_v4();
        let record = journal
            .prepare_files(operation_id, &instance, "file-owner-plan".into())
            .unwrap();
        drop(journal);

        let journal = DeletionJournal {
            registry: Registry::new(Arc::new(MetadataStore::open(&metadata).unwrap())),
        };
        let mut recovered = journal.get(operation_id).unwrap().unwrap();
        assert_eq!(recovered.instance, record.instance);
        assert_eq!(recovered.park_receipt, record.park_receipt);
        assert_eq!(recovered.snapshot().status, DeletionStatus::PendingRestore);
        journal.commit(&mut recovered).unwrap();
        drop(journal);

        let journal = DeletionJournal {
            registry: Registry::new(Arc::new(MetadataStore::open(&metadata).unwrap())),
        };
        let recovered = journal.get(operation_id).unwrap().unwrap();
        assert_eq!(recovered.snapshot().status, DeletionStatus::CleanupPending);
        assert!(journal.registry.list().unwrap().is_empty());
    }

    async fn prepared_deletion(
        service: &InstanceService,
    ) -> (DeletionJournal, DeletionRecord, PathBuf, PathBuf) {
        let instance = creation::create(service, "Recovery fixture").await;
        let source = creation::payload_path(service, &instance.id);
        std::fs::write(source.join("options.txt"), b"user-owned settings").unwrap();
        let admitted = service.directories.admit(&instance.id).unwrap();
        let parent = super::super::directory::instances_parent(admitted.generation()).unwrap();
        let operation = uuid::Uuid::new_v4();
        let plan = parent
            .plan_park(
                &PortableName::new_exact(&admitted.record().directory_name).unwrap(),
                &PortableName::new_exact(&format!("deleted-{operation}")).unwrap(),
            )
            .unwrap();
        let parked = source.with_file_name(plan.receipt().park_name());
        let journal = DeletionJournal {
            registry: service.registry().clone(),
        };
        let record = journal
            .prepare_files(operation, admitted.record(), plan.receipt().encode())
            .unwrap();
        // The tests arrange interrupted disk states after this real plan drops;
        // recovery must reconstruct authority from its persisted native witness.
        (journal, record, source, parked)
    }

    async fn queued_setup(service: &InstanceService) -> RegisteredInstance {
        service
            .create_admitted(
                creation::request("Queued setup"),
                creation::target(),
                service.creation_admission_for_tests().await.unwrap(),
                super::super::create::SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: "{}".into(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn pristine_setup_removal_reuses_admission_and_commits_setup_removal() {
        let (_root, service) = creation::fixture();
        let admitted = queued_setup(&service).await;
        let id = admitted.record().instance.id.clone();
        let source = creation::payload_path(&service, &id);
        service.registry().select(&id).unwrap();
        let operation = uuid::Uuid::new_v4();
        let removed = service
            .delete_pristine_setup_admitted(admitted.clone(), operation)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(removed.status, DeletionStatus::Removed);
        assert_eq!(removed.instance_id, id);
        assert!(!source.exists());
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.registry().last_instance_id().unwrap().is_none());
        assert!(!super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
        assert!(service.pending_deletions().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
        assert_eq!(
            service
                .delete_pristine_setup_admitted(admitted, operation)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap(),
            Some(removed)
        );
    }

    #[tokio::test]
    async fn changed_setup_payload_is_preserved_without_deletion_intent() {
        for change in [
            "root file",
            "nested file",
            "extra directory",
            "missing directory",
            "replaced root",
        ] {
            let (_root, service) = creation::fixture();
            let admitted = queued_setup(&service).await;
            let id = admitted.record().instance.id.clone();
            let source = creation::payload_path(&service, &id);
            let displaced = source.with_file_name("preserved-original");
            let canary = match change {
                "root file" => Some(source.join("options.txt")),
                "nested file" => Some(source.join("config").join("user.cfg")),
                "extra directory" => {
                    std::fs::create_dir(source.join("custom")).unwrap();
                    None
                }
                "missing directory" => {
                    std::fs::remove_dir(source.join("mods")).unwrap();
                    None
                }
                "replaced root" => {
                    std::fs::rename(&source, &displaced).unwrap();
                    std::fs::create_dir(&source).unwrap();
                    Some(source.join("foreign-canary"))
                }
                _ => unreachable!(),
            };
            if let Some(canary) = &canary {
                std::fs::write(canary, b"user data").unwrap();
            }
            let operation = uuid::Uuid::new_v4();
            assert!(
                service
                    .delete_pristine_setup_admitted(admitted, operation)
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none(),
                "{change}"
            );
            if let Some(canary) = canary {
                assert_eq!(std::fs::read(canary).unwrap(), b"user data");
            }
            assert!(source.exists());
            if change == "extra directory" {
                assert!(source.join("custom").is_dir());
            }
            if change == "missing directory" {
                assert!(!source.join("mods").exists());
            }
            if change == "replaced root" {
                assert!(displaced.join("mods").is_dir());
            }
            assert!(service.registry().get_live(&id).is_ok());
            assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
            assert!(matches!(
                service.deletion_status(operation),
                Err(DeletionError::NotFound)
            ));
            assert!(!service.has_unsettled_effects());
            assert!(matches!(
                service.directories.admit(&id),
                Err(InstanceError::Busy)
            ));
        }
    }

    #[tokio::test]
    async fn changed_setup_metadata_preserves_even_an_empty_payload() {
        let (_root, service) = creation::fixture();
        let admitted = queued_setup(&service).await;
        let id = admitted.record().instance.id.clone();
        let updated = service
            .registry()
            .update(
                &id,
                admitted.record().revision,
                super::super::model::InstancePatch {
                    name: Some("User renamed setup".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let admitted = super::super::directory::InstanceDirectories::admit_record(
            service.registry().clone(),
            updated.clone(),
            admitted.generation().clone(),
            admitted.lease().clone(),
        )
        .unwrap();
        assert!(
            service
                .delete_pristine_setup_admitted(admitted, uuid::Uuid::new_v4())
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert_eq!(service.registry().get_live(&id).unwrap(), updated);
        assert!(creation::payload_path(&service, &id).join("mods").is_dir());
        assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
        assert!(service.pending_deletions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pristine_proof_rechecks_child_contents_and_identity_before_parking() {
        for replace in [false, true] {
            let (_root, service) = creation::fixture();
            let admitted = queued_setup(&service).await;
            let id = admitted.record().instance.id.clone();
            let source = creation::payload_path(&service, &id);
            let proof = service.pristine_setup_proof(&admitted).unwrap().unwrap();
            let config = source.join("config");
            if replace {
                std::fs::rename(&config, source.with_file_name("original-config")).unwrap();
                std::fs::create_dir(&config).unwrap();
            } else {
                std::fs::write(config.join("user.cfg"), b"new user data").unwrap();
            }
            let operation = uuid::Uuid::new_v4();
            assert!(matches!(
                service.delete_files_admitted(
                    DeletionJournal {
                        registry: service.registry().clone()
                    },
                    admitted,
                    operation,
                    &crate::tasks::CancellationToken::new(),
                    Some(proof),
                ),
                Err(DeletionError::FilesPreserved)
            ));
            assert!(config.is_dir());
            if !replace {
                assert_eq!(
                    std::fs::read(config.join("user.cfg")).unwrap(),
                    b"new user data"
                );
            }
            assert!(service.registry().get_live(&id).is_ok());
            assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
            assert!(matches!(
                service.deletion_status(operation),
                Err(DeletionError::NotFound)
            ));
            assert!(
                !source
                    .with_file_name(format!("deleted-{operation}"))
                    .exists()
            );
        }
    }

    #[tokio::test]
    async fn failed_setup_commit_restores_payload_registry_and_pending_setup() {
        let (_root, service) = creation::fixture();
        let admitted = queued_setup(&service).await;
        let id = admitted.record().instance.id.clone();
        let source = creation::payload_path(&service, &id);
        service.registry().select(&id).unwrap();
        service
            .registry()
            .storage()
            .transaction(|db| {
                db.execute_batch(
                    "CREATE TRIGGER refuse_setup_removal BEFORE DELETE ON instance_setups
                 BEGIN SELECT RAISE(ABORT, 'injected setup commit failure'); END;",
                )
                .map_err(StorageError::from)
            })
            .unwrap();
        let operation = uuid::Uuid::new_v4();
        assert!(matches!(
            service
                .delete_pristine_setup_admitted(admitted, operation)
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(DeletionError::Storage(_))
        ));
        assert!(source.join("mods").is_dir());
        assert!(
            !source
                .with_file_name(format!("deleted-{operation}"))
                .exists()
        );
        let restored = service.directories.admit_for_setup_settlement(&id).unwrap();
        restored.validate_current().unwrap();
        assert_eq!(
            service.registry().last_instance_id().unwrap(),
            Some(id.clone())
        );
        assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
        assert_eq!(
            service.deletion_status(operation).unwrap().status,
            DeletionStatus::Aborted
        );
        assert!(service.pending_deletions().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn dropping_setup_removal_waiter_does_not_abandon_accepted_cleanup() {
        let (_root, service) = creation::fixture();
        let admitted = queued_setup(&service).await;
        let id = admitted.record().instance.id.clone();
        let source = creation::payload_path(&service, &id);
        let operation = uuid::Uuid::new_v4();
        drop(
            service
                .delete_pristine_setup_admitted(admitted, operation)
                .unwrap(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if service
                    .deletion_status(operation)
                    .is_ok_and(|snapshot| snapshot.status == DeletionStatus::Removed)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!source.exists());
        assert!(service.registry().get_live(&id).is_err());
        assert!(!super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
    }

    #[tokio::test]
    async fn explicit_setup_removal_honors_file_intent_after_active_admission_releases() {
        for intent in [DeleteIntent::KeepFiles, DeleteIntent::DeleteFiles] {
            let (_root, service) = creation::fixture();
            let admitted = queued_setup(&service).await;
            let id = admitted.record().instance.id.clone();
            let source = creation::payload_path(&service, &id);
            let user_file = source.join("config").join("user.cfg");
            std::fs::write(&user_file, b"modified after setup creation").unwrap();
            assert!(matches!(
                service.delete(&id, intent, uuid::Uuid::new_v4()),
                Err(DeletionError::Instance(InstanceError::Busy))
            ));
            assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
            drop(admitted);
            let operation = uuid::Uuid::new_v4();
            let removed = service
                .delete(&id, intent, operation)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(removed.status, DeletionStatus::Removed);
            assert_eq!(removed.intent, intent);
            if intent == DeleteIntent::KeepFiles {
                assert_eq!(
                    std::fs::read(&user_file).unwrap(),
                    b"modified after setup creation"
                );
            } else {
                assert!(!source.exists());
            }
            assert!(matches!(
                service.registry().get_record(&id),
                Err(InstanceError::NotFound)
            ));
            assert!(!super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
            assert!(!service.has_pending_intents());
            assert_eq!(
                service
                    .delete(&id, intent, operation)
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
                    .unwrap(),
                removed
            );
        }
    }

    #[tokio::test]
    async fn explicit_setup_removal_never_discards_unverified_content_or_performance_effects() {
        for content in [true, false] {
            for intent in [DeleteIntent::KeepFiles, DeleteIntent::DeleteFiles] {
                let (_root, service) = creation::fixture();
                let admitted = queued_setup(&service).await;
                let id = admitted.record().instance.id.clone();
                let source = creation::payload_path(&service, &id);
                std::fs::write(source.join("options.txt"), b"user data").unwrap();
                drop(admitted);
                // Unknown durable receipts must fence deletion, not be parsed
                // as authority or discarded merely because setup is pending.
                service.registry().storage().transaction(|tx| {
                    if content {
                        tx.execute("INSERT INTO content_batches(instance_id,operation_id,receipt_json) VALUES(?1,?2,'unverified')",
                            params![id.as_str(), uuid::Uuid::new_v4().to_string()])?;
                    } else {
                        tx.execute("INSERT INTO performance_operations(instance_id,operation_id,payload) VALUES(?1,?2,?3)",
                            params![id.as_str(), uuid::Uuid::new_v4().to_string(), b"unverified".as_slice()])?;
                    }
                    Ok::<_, StorageError>(())
                }).unwrap();
                let operation = uuid::Uuid::new_v4();
                assert!(matches!(
                    service.delete(&id, intent, operation),
                    Err(DeletionError::Instance(InstanceError::Busy))
                ));
                assert_eq!(
                    std::fs::read(source.join("options.txt")).unwrap(),
                    b"user data"
                );
                assert!(service.registry().get_live(&id).is_ok());
                assert!(
                    super::super::setup::has_pending(service.registry().storage(), &id).unwrap()
                );
                assert!(if content {
                    crate::content::install::has_pending(service.registry().storage(), &id).unwrap()
                } else {
                    crate::performance::mutation::has_pending(service.registry().storage(), &id)
                        .unwrap()
                });
                assert!(matches!(
                    service.deletion_status(operation),
                    Err(DeletionError::NotFound)
                ));
            }
        }
    }

    #[tokio::test]
    async fn explicit_setup_removal_failure_preserves_files_setup_and_selection() {
        for intent in [DeleteIntent::KeepFiles, DeleteIntent::DeleteFiles] {
            let (_root, service) = creation::fixture();
            let admitted = queued_setup(&service).await;
            let id = admitted.record().instance.id.clone();
            let source = creation::payload_path(&service, &id);
            std::fs::write(source.join("options.txt"), b"user data").unwrap();
            service.registry().select(&id).unwrap();
            drop(admitted);
            service
                .registry()
                .storage()
                .transaction(|tx| {
                    tx.execute_batch(
                        "CREATE TRIGGER refuse_setup_removal BEFORE DELETE ON instance_setups
                     BEGIN SELECT RAISE(ABORT, 'injected setup commit failure'); END;",
                    )
                    .map_err(StorageError::from)
                })
                .unwrap();
            let operation = uuid::Uuid::new_v4();
            assert!(matches!(
                service
                    .delete(&id, intent, operation)
                    .unwrap()
                    .join()
                    .await
                    .unwrap(),
                Err(DeletionError::Storage(_))
            ));
            assert_eq!(
                std::fs::read(source.join("options.txt")).unwrap(),
                b"user data"
            );
            service
                .directories
                .admit_for_setup_settlement(&id)
                .unwrap()
                .validate_current()
                .unwrap();
            assert_eq!(
                service.registry().last_instance_id().unwrap(),
                Some(id.clone())
            );
            assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
            if intent == DeleteIntent::KeepFiles {
                assert!(matches!(
                    service.deletion_status(operation),
                    Err(DeletionError::NotFound)
                ));
            } else {
                assert_eq!(
                    service.deletion_status(operation).unwrap().status,
                    DeletionStatus::Aborted
                );
            }
            assert!(service.pending_deletions().unwrap().is_empty());
            assert!(!service.has_unsettled_effects());
        }
    }

    #[tokio::test]
    async fn prepared_park_restores_payload_and_registry_on_retry() {
        let (_root, service) = creation::fixture();
        let (_journal, record, source, parked) = prepared_deletion(&service).await;
        std::fs::rename(&source, &parked).unwrap();
        let id = &record.instance.instance.id;
        let restored = service
            .delete(id, DeleteIntent::DeleteFiles, record.operation_id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.status, DeletionStatus::Aborted);
        assert_eq!(
            std::fs::read(source.join("options.txt")).unwrap(),
            b"user-owned settings"
        );
        assert!(!parked.exists());
        let admitted = service.directories.admit(id).unwrap();
        admitted.validate_current().unwrap();
        assert_eq!(
            admitted.record().directory_receipt,
            record.instance.directory_receipt
        );
        assert!(service.pending_deletions().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn committed_park_cleanup_preserves_unrelated_payload_on_retry() {
        let (_root, service) = creation::fixture();
        let (journal, mut record, source, parked) = prepared_deletion(&service).await;
        let unrelated = source.with_file_name("unrelated-world");
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("canary"), b"foreign payload").unwrap();
        std::fs::rename(&source, &parked).unwrap();
        journal.commit(&mut record).unwrap();
        let removed = service
            .delete(
                &record.instance.instance.id,
                DeleteIntent::DeleteFiles,
                record.operation_id,
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(removed.status, DeletionStatus::Removed);
        assert!(!source.exists());
        assert!(!parked.exists());
        assert_eq!(
            std::fs::read(unrelated.join("canary")).unwrap(),
            b"foreign payload"
        );
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.pending_deletions().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn missing_source_keeps_precommit_pending_and_completes_committed_cleanup() {
        for committed in [false, true] {
            let (_root, service) = creation::fixture();
            let (journal, mut record, source, parked) = prepared_deletion(&service).await;
            std::fs::rename(&source, &parked).unwrap();
            if committed {
                journal.commit(&mut record).unwrap();
            }
            let displaced = source.with_file_name("moved-by-user");
            std::fs::rename(&parked, &displaced).unwrap();
            let result = service
                .delete(
                    &record.instance.instance.id,
                    DeleteIntent::DeleteFiles,
                    record.operation_id,
                )
                .unwrap()
                .join()
                .await
                .unwrap();
            if committed {
                assert_eq!(result.unwrap().status, DeletionStatus::Removed);
                assert!(service.pending_deletions().unwrap().is_empty());
                assert!(!service.has_unsettled_effects());
            } else {
                assert!(matches!(result, Err(DeletionError::FilesPreserved)));
                assert_pending_deletion(&service, &record);
            }
            assert!(!source.exists());
            assert!(!parked.exists());
            assert_eq!(
                std::fs::read(displaced.join("options.txt")).unwrap(),
                b"user-owned settings"
            );
        }
    }

    #[tokio::test]
    async fn replacement_source_and_park_preserve_foreign_bytes_and_pending_intent() {
        for committed in [false, true] {
            for replace_source in [false, true] {
                let (_root, service) = creation::fixture();
                let (journal, mut record, source, parked) = prepared_deletion(&service).await;
                std::fs::rename(&source, &parked).unwrap();
                if committed {
                    journal.commit(&mut record).unwrap();
                }
                let displaced = source.with_file_name("moved-by-user");
                std::fs::rename(&parked, &displaced).unwrap();
                let replacement = if replace_source { &source } else { &parked };
                std::fs::create_dir(replacement).unwrap();
                std::fs::write(replacement.join("canary"), b"foreign payload").unwrap();
                let result = service
                    .delete(
                        &record.instance.instance.id,
                        DeleteIntent::DeleteFiles,
                        record.operation_id,
                    )
                    .unwrap()
                    .join()
                    .await
                    .unwrap();
                assert!(matches!(result, Err(DeletionError::FilesPreserved)));
                assert_pending_deletion(&service, &record);
                assert_eq!(
                    std::fs::read(replacement.join("canary")).unwrap(),
                    b"foreign payload"
                );
                assert_eq!(
                    std::fs::read(displaced.join("options.txt")).unwrap(),
                    b"user-owned settings"
                );
                assert_eq!(source.exists(), replace_source);
                assert_eq!(parked.exists(), !replace_source);
            }
        }
    }

    #[tokio::test]
    async fn committed_deletion_never_cleans_an_original_returned_to_its_source_name() {
        let (_root, service) = creation::fixture();
        let (journal, mut record, source, parked) = prepared_deletion(&service).await;
        std::fs::rename(&source, &parked).unwrap();
        journal.commit(&mut record).unwrap();
        std::fs::rename(&parked, &source).unwrap();
        let result = service
            .delete(
                &record.instance.instance.id,
                DeleteIntent::DeleteFiles,
                record.operation_id,
            )
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(matches!(result, Err(DeletionError::FilesPreserved)));
        assert_pending_deletion(&service, &record);
        assert_eq!(
            std::fs::read(source.join("options.txt")).unwrap(),
            b"user-owned settings"
        );
        assert!(!parked.exists());
    }

    fn assert_pending_deletion(service: &InstanceService, record: &DeletionRecord) {
        assert!(service.registry().list().unwrap().is_empty());
        assert_eq!(service.pending_deletions().unwrap(), [record.snapshot()]);
        assert_eq!(
            service.deletion_status(record.operation_id).unwrap(),
            record.snapshot()
        );
        assert!(service.has_unsettled_effects());
        assert!(matches!(
            service.delete(
                &record.instance.instance.id,
                DeleteIntent::KeepFiles,
                uuid::Uuid::new_v4()
            ),
            Err(DeletionError::Instance(InstanceError::Busy))
        ));
        assert!(
            service
                .directories
                .exclusions()
                .try_acquire([record.instance.instance.id.as_str()], [])
                .is_err()
        );
    }

    #[test]
    fn file_deletion_intent_has_no_default_or_ambiguous_value() {
        assert!(serde_json::from_str::<DeleteIntent>("null").is_err());
        assert!(serde_json::from_str::<DeleteIntent>("true").is_err());
        assert_eq!(
            serde_json::from_str::<DeleteIntent>("\"keep_files\"").unwrap(),
            DeleteIntent::KeepFiles
        );
        assert_eq!(
            serde_json::from_str::<DeleteIntent>("\"delete_files\"").unwrap(),
            DeleteIntent::DeleteFiles
        );
    }
}
