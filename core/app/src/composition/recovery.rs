//! Recovery-only owner of recorded composition operations.
//!
//! The current application records no new composition work. It settles what
//! an earlier run of it recorded: each pending operation is proven against
//! the instance's recovered managed files and finished as applied or
//! interrupted only on that evidence; anything else stays reserved and
//! unresolved. Launch admission proves a composition absent or settled.

use super::model::{BundleHealth, CompositionState};
use super::rollback::{ExpectedComposition, RestartDisposition, classify_recovered};
use super::{
    ManagedArtifactWitnessProof, ManagedCompositionAuthority, ManagedCompositionInspection,
    ManagedInstanceEffectAuthority, ManagedInstanceIdentity,
};
use crate::{
    instances::{
        directory::{InstanceDirectories, RegisteredInstance},
        model::InstanceId,
    },
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{self, Connection, OptionalExtension, Transaction, params},
    },
    tasks::TaskOwner,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub const MIGRATION: Migration = Migration {
    id: "performance_operations.v1",
    sql: "CREATE TABLE performance_operations (instance_id TEXT PRIMARY KEY NOT NULL, operation_id TEXT UNIQUE NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<=2097152)) STRICT;
    CREATE TABLE performance_commands (id TEXT PRIMARY KEY NOT NULL, instance_id TEXT NOT NULL, state TEXT NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<=16384)) STRICT;
    CREATE INDEX performance_commands_active ON performance_commands(state) WHERE state IN ('queued','running');",
};

const MAX_PENDING_OPERATIONS: usize = 128;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_ROW_BYTES: usize = 2 * 1024 * 1024;
const MAX_ACTIVE_COMMANDS: usize = 128;
const MAX_COMMAND_BYTES: usize = 16 * 1024;

pub fn has_pending(storage: &MetadataStore, id: &InstanceId) -> Result<bool, StorageError> {
    storage.read(|connection| {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM performance_operations WHERE instance_id=?1)",
                [id.as_str()],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    })
}

#[derive(Debug, thiserror::Error)]
pub enum CompositionError {
    #[error("performance storage is unavailable")]
    Storage(#[from] StorageError),
    #[error("instance was not found")]
    InstanceNotFound,
    #[error("instance is busy or unavailable")]
    InstanceUnavailable,
    #[error("performance work is still settling; the instance remains reserved")]
    Unsettled,
    #[error("performance operation failed and its previous state was preserved")]
    Failed,
    #[error("performance operation was cancelled before publication")]
    Cancelled,
    #[error("performance rules or dependency plan are unavailable")]
    PlanUnavailable,
    #[error("performance rollback snapshot was not found")]
    SnapshotNotFound,
    #[error("performance rollback snapshot is not available")]
    SnapshotUnavailable,
}

impl CompositionError {
    /// Static diagnostics only: storage and provider error payloads may contain
    /// paths or remote response text and must not reach launch logs.
    pub(crate) fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage_unavailable",
            Self::InstanceNotFound => "instance_not_found",
            Self::InstanceUnavailable => "instance_unavailable",
            Self::Unsettled => "unsettled",
            Self::Failed => "failed_preserved",
            Self::Cancelled => "cancelled",
            Self::PlanUnavailable => "plan_unavailable",
            Self::SnapshotNotFound => "snapshot_not_found",
            Self::SnapshotUnavailable => "snapshot_unavailable",
        }
    }
}

impl From<rusqlite::Error> for CompositionError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingOperation {
    operation_id: String,
    instance_id: InstanceId,
    directory_receipt: String,
    before: Option<CompositionState>,
    expected: ExpectedComposition,
    target_effect_started: bool,
    result: Option<CompletedComposition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "state", rename_all = "snake_case")]
enum CompletedComposition {
    Absent,
    Managed(CompositionState),
}
impl CompletedComposition {
    fn state(&self) -> Option<CompositionState> {
        match self {
            Self::Absent => None,
            Self::Managed(state) => Some(state.clone()),
        }
    }
}

#[derive(Clone)]
struct BoundInstance {
    instance: RegisteredInstance,
    authority: ManagedCompositionAuthority,
    identity: ManagedInstanceIdentity,
    effects: ManagedInstanceEffectAuthority,
}

struct RetainedOperation {
    instance: RegisteredInstance,
    bound: Option<BoundInstance>,
    claimed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOperationStatus {
    pub id: String,
    pub instance_id: String,
    pub action: String,
    pub state: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn bounded_command_row(
    db: &Connection,
    id: &str,
) -> Result<Option<(String, String, Vec<u8>)>, StorageError> {
    let row: Option<(Option<String>, Option<String>, Option<Vec<u8>>)> = db.query_row(
        "SELECT CASE WHEN length(CAST(instance_id AS BLOB))<=36 THEN instance_id END,CASE WHEN length(CAST(state AS BLOB))<=32 THEN state END,CASE WHEN length(CAST(payload AS BLOB))<=?2 THEN payload END FROM performance_commands WHERE id=?1",
        params![id, MAX_COMMAND_BYTES],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    row.map(|(instance, state, bytes)| {
        let bytes = bytes.ok_or(StorageError::Corrupt)?;
        Ok((
            instance.ok_or(StorageError::Corrupt)?,
            state.ok_or(StorageError::Corrupt)?,
            bytes,
        ))
    })
    .transpose()
}

fn command_status(
    id: &str,
    instance: &str,
    state: &str,
    bytes: &[u8],
) -> Result<PerformanceOperationStatus, CompositionError> {
    let status: PerformanceOperationStatus =
        serde_json::from_slice(bytes).map_err(|_| CompositionError::Unsettled)?;
    validate_command(&status)?;
    if status.id != id || status.instance_id != instance || status.state != state {
        return Err(CompositionError::Unsettled);
    }
    Ok(status)
}

fn pending_command(
    connection: &Connection,
    pending: &PendingOperation,
) -> Result<Option<PerformanceOperationStatus>, CompositionError> {
    let record: Option<(String, String, Vec<u8>)> = connection
        .query_row(
            "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
            [&pending.operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((instance, state, bytes)) = record else {
        return Ok(None);
    };
    let status = command_status(&pending.operation_id, &instance, &state, &bytes)?;
    let action_matches = matches!(
        (&pending.expected, status.action.as_str()),
        (ExpectedComposition::Graph { .. }, "apply" | "reapply")
            | (ExpectedComposition::Absent, "apply" | "reapply" | "remove")
            | (ExpectedComposition::Snapshot { .. }, "rollback")
    );
    if status.instance_id != pending.instance_id.as_str()
        || uuid::Uuid::parse_str(&status.id).is_err()
        || !matches!(status.state.as_str(), "running" | "unsettled")
        || !action_matches
    {
        return Err(CompositionError::Unsettled);
    }
    Ok(Some(status))
}

fn read_pending(
    connection: &Connection,
    instance: &InstanceId,
) -> Result<Option<PendingOperation>, CompositionError> {
    let record: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT operation_id,payload FROM performance_operations WHERE instance_id=?1",
            [instance.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    record
        .map(|(id, bytes)| decode_pending(instance.as_str(), &id, &bytes))
        .transpose()
}

fn decode_pending(
    instance: &str,
    id: &str,
    bytes: &[u8],
) -> Result<PendingOperation, CompositionError> {
    let pending: PendingOperation =
        serde_json::from_slice(bytes).map_err(|_| CompositionError::Unsettled)?;
    if pending.instance_id.as_str() != instance
        || pending.operation_id != id
        || uuid::Uuid::parse_str(id).is_err()
    {
        return Err(CompositionError::Unsettled);
    }
    Ok(pending)
}

fn check_pending_budget(count: usize, bytes: usize) -> Result<(), CompositionError> {
    if count > MAX_PENDING_OPERATIONS || bytes > MAX_PENDING_BYTES {
        return Err(CompositionError::Unsettled);
    }
    Ok(())
}

fn pending_inventory(connection: &Connection) -> Result<(usize, usize), CompositionError> {
    let mut query =
        connection.prepare("SELECT length(payload) FROM performance_operations LIMIT ?1")?;
    let mut rows = query.query([MAX_PENDING_OPERATIONS + 1])?;
    let (mut count, mut bytes) = (0, 0usize);
    while let Some(row) = rows.next()? {
        let length: usize = row.get(0)?;
        if length > MAX_PENDING_ROW_BYTES {
            return Err(CompositionError::Unsettled);
        }
        count += 1;
        bytes = bytes
            .checked_add(length)
            .ok_or(CompositionError::Unsettled)?;
        check_pending_budget(count, bytes)?;
    }
    Ok((count, bytes))
}

fn validate_command(status: &PerformanceOperationStatus) -> Result<(), CompositionError> {
    if uuid::Uuid::parse_str(&status.id).is_err()
        || status.instance_id.parse::<InstanceId>().is_err()
        || !matches!(
            status.action.as_str(),
            "apply" | "reapply" | "remove" | "rollback"
        )
        || !matches!(
            status.state.as_str(),
            "queued" | "running" | "complete" | "failed" | "interrupted" | "unsettled"
        )
    {
        return Err(CompositionError::Unsettled);
    }
    Ok(())
}

fn write_command(
    tx: &Transaction<'_>,
    status: &PerformanceOperationStatus,
) -> Result<(), CompositionError> {
    validate_command(status)?;
    let previous: Option<(String, String, Vec<u8>)> = tx
        .query_row(
            "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
            [&status.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let admits_active = matches!(status.state.as_str(), "queued" | "running")
        && !previous
            .as_ref()
            .is_some_and(|(_, state, _)| matches!(state.as_str(), "queued" | "running"));
    if admits_active && active_command_count(tx)? >= MAX_ACTIVE_COMMANDS {
        return Err(CompositionError::Unsettled);
    }
    if let Some((instance, state, bytes)) = previous {
        let previous = command_status(&status.id, &instance, &state, &bytes)?;
        if previous.instance_id != status.instance_id
            || previous.action != status.action
            || previous.created_at != status.created_at
            || (matches!(
                previous.state.as_str(),
                "complete" | "failed" | "interrupted"
            ) && (previous.state != status.state || previous.error != status.error))
        {
            return Err(CompositionError::Unsettled);
        }
    }
    let bytes = serde_json::to_vec(status).map_err(|_| CompositionError::Unsettled)?;
    if tx.execute("INSERT INTO performance_commands(id,instance_id,state,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET state=excluded.state,payload=excluded.payload", params![status.id, status.instance_id, status.state, bytes])? != 1 {
        return Err(CompositionError::Unsettled);
    }
    let saved: (String, String, Vec<u8>) = tx.query_row(
        "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
        [&status.id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if saved != (status.instance_id.clone(), status.state.clone(), bytes) {
        return Err(CompositionError::Unsettled);
    }
    if admits_active && active_command_count(tx)? > MAX_ACTIVE_COMMANDS {
        return Err(CompositionError::Unsettled);
    }
    Ok(())
}

fn active_command_count(connection: &Connection) -> Result<usize, CompositionError> {
    Ok(connection.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM performance_commands WHERE state IN ('queued','running') LIMIT ?1)",
        [MAX_ACTIVE_COMMANDS + 1],
        |row| row.get(0),
    )?)
}

#[derive(Clone)]
pub struct CompositionService {
    storage: Arc<MetadataStore>,
    instances: InstanceDirectories,
    tasks: TaskOwner,
    retained: Arc<Mutex<BTreeMap<String, RetainedOperation>>>,
}

/// Owner proof that an admitted instance's composition is absent or fully
/// settled: no pending operation, queued or running command or retained
/// effect, and the instance's managed files proven against their recorded
/// state. Launch holds the instance lease, which excludes new composition
/// work; `revalidate` repeats the whole proof immediately before spawn.
#[derive(Clone)]
pub struct CompositionSettled {
    service: CompositionService,
    instance: RegisteredInstance,
    state: Option<CompositionState>,
}

impl CompositionSettled {
    /// The cheap owner checks: instance binding and the owner's records.
    pub fn validate_current(&self) -> Result<(), CompositionError> {
        self.instance
            .validate_current()
            .map_err(|_| CompositionError::InstanceUnavailable)?;
        self.service
            .check_settled(&self.instance.record().instance.id)
    }

    /// Repeats the complete proof and requires the same recorded state.
    pub async fn revalidate(&self) -> Result<(), CompositionError> {
        let proof = self.clone();
        axial_resource::process_physical_work()
            .admit(axial_resource::PhysicalWorkRequest::foreground(
                axial_resource::PhysicalIoClass::Read,
                0,
            ))
            .await
            .map_err(|_| CompositionError::Unsettled)?
            .run_parallel(move |_, workers| proof.revalidate_admitted(&workers))
            .await
            .map_err(|_| CompositionError::Unsettled)?
    }

    /// The spawn admission includes this proof; it never admits nested work.
    pub(crate) fn revalidate_admitted(
        &self,
        workers: &axial_resource::PhysicalWorkParallelism,
    ) -> Result<(), CompositionError> {
        self.validate_current()?;
        // Effect authority is bound only for the proof, never retained by
        // the prepared session.
        let bound = self.service.bind_admitted(self.instance.clone(), workers)?;
        let inspection = bound
            .authority
            .prove_settled_now(&bound.identity, &bound.effects)
            .map_err(|_| CompositionError::Unsettled)?;
        drop(bound);
        if inspection.health == BundleHealth::Invalid || inspection.state != self.state {
            return Err(CompositionError::Unsettled);
        }
        self.validate_current()
    }
}

impl CompositionService {
    pub fn new(
        storage: Arc<MetadataStore>,
        instances: InstanceDirectories,
        tasks: TaskOwner,
    ) -> Result<Self, CompositionError> {
        let service = Self {
            storage,
            instances,
            tasks,
            retained: Arc::new(Mutex::new(BTreeMap::new())),
        };
        // Reserve all interrupted targets before other features admit work.
        // Corrupt records fail startup instead of silently releasing exclusion.
        for pending in service.pending()? {
            service.storage.read(|db| pending_command(db, &pending))?;
            let instance = service
                .instances
                .admit_for_performance_settlement(&pending.instance_id)
                .map_err(|_| CompositionError::Unsettled)?;
            instance
                .directory()
                .verify_receipt(&pending.directory_receipt)
                .map_err(|_| CompositionError::Unsettled)?;
            service.retain(instance, None);
        }
        service.storage.transaction(|tx| {
            let mut query = tx.prepare(
                "SELECT id,instance_id,state,payload FROM performance_commands WHERE state IN ('queued','running') LIMIT ?1",
            )?;
            let mut rows = query.query([MAX_ACTIVE_COMMANDS + 1])?;
            let mut records = Vec::new();
            while let Some(row) = rows.next()? {
                if records.len() == MAX_ACTIVE_COMMANDS {
                    return Err(CompositionError::Unsettled);
                }
                let id = row.get_ref(0)?.as_str().map_err(|_| CompositionError::Unsettled)?;
                let instance = row.get_ref(1)?.as_str().map_err(|_| CompositionError::Unsettled)?;
                let state = row.get_ref(2)?.as_str().map_err(|_| CompositionError::Unsettled)?;
                let bytes = row.get_ref(3)?.as_blob().map_err(|_| CompositionError::Unsettled)?;
                records.push(command_status(id, instance, state, bytes)?);
            }
            drop(rows);
            drop(query);
            for mut status in records {
                let instance_id = status.instance_id.parse().map_err(|_| CompositionError::Unsettled)?;
                if read_pending(tx, &instance_id)?.is_some_and(|pending| pending.operation_id == status.id) {
                    continue;
                }
                status.state = "interrupted".into();
                status.error =
                    Some("Operation interrupted; inspect the instance before retrying".into());
                write_command(tx, &status)?;
            }
            Ok::<_, CompositionError>(())
        })?;
        Ok(service)
    }

    /// Fixture preparation: records a removal of an absent composition and
    /// its running command exactly as an earlier run of this application
    /// wrote them, before any target effect. Returns the command identity.
    #[cfg(feature = "test-support")]
    pub fn record_unstarted_removal_for_test(
        &self,
        id: &InstanceId,
    ) -> Result<String, CompositionError> {
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| CompositionError::InstanceUnavailable)?;
        let now = chrono::Utc::now().to_rfc3339();
        let command = PerformanceOperationStatus {
            id: uuid::Uuid::new_v4().to_string(),
            instance_id: id.to_string(),
            action: "remove".into(),
            state: "running".into(),
            error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        let pending = PendingOperation {
            operation_id: command.id.clone(),
            instance_id: id.clone(),
            directory_receipt: instance
                .directory()
                .receipt()
                .map_err(|_| CompositionError::Unsettled)?,
            before: None,
            expected: ExpectedComposition::Absent,
            target_effect_started: false,
            result: None,
        };
        let bytes = serde_json::to_vec(&pending).map_err(|_| CompositionError::Unsettled)?;
        self.storage.transaction(|tx| {
            write_command(tx, &command)?;
            tx.execute(
                "INSERT INTO performance_operations(instance_id,operation_id,payload) VALUES(?1,?2,?3)",
                params![id.as_str(), pending.operation_id, bytes],
            )?;
            Ok::<_, CompositionError>(())
        })?;
        Ok(command.id)
    }

    pub fn instances(&self) -> &InstanceDirectories {
        &self.instances
    }
    pub fn has_unsettled_effects(&self) -> bool {
        !self
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
    }
    pub fn pending_count(&self) -> Result<usize, CompositionError> {
        Ok(self.pending()?.len())
    }

    /// Read-only ownership evidence under the consumer's existing admission.
    /// Empty is authoritative only after a successfully admitted absent state;
    /// corrupt metadata and missing authority return an error, never an empty set.
    pub async fn managed_witness_proofs(
        &self,
        instance: &RegisteredInstance,
    ) -> Result<Vec<ManagedArtifactWitnessProof>, CompositionError> {
        let instance = instance.clone();
        let service = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |_| async move {
                let bound = service.bind(instance).await?;
                bound
                    .authority
                    .composition_managed_witness_proofs(&bound.identity, &bound.effects)
                    .await
                    .map_err(|_| CompositionError::Unsettled)
            })
            .map_err(|_| CompositionError::InstanceUnavailable)?
            .join()
            .await
            .map_err(|_| CompositionError::Unsettled)?
    }
    pub fn operation(
        &self,
        id: &str,
    ) -> Result<Option<PerformanceOperationStatus>, CompositionError> {
        self.storage.read(|connection| {
            let record = bounded_command_row(connection, id)?;
            record
                .map(|(instance, state, bytes)| command_status(id, &instance, &state, &bytes))
                .transpose()
        })
    }

    pub async fn settled_for_launch(
        &self,
        instance: &RegisteredInstance,
    ) -> Result<CompositionSettled, CompositionError> {
        instance
            .validate_current()
            .map_err(|_| CompositionError::InstanceUnavailable)?;
        self.check_settled(&instance.record().instance.id)?;
        let bound = self.bind(instance.clone()).await?;
        let inspection = match bound
            .authority
            .prove_settled(&bound.identity, &bound.effects)
            .await
        {
            Ok(inspection) if inspection.health != BundleHealth::Invalid => inspection,
            _ => {
                let inspection = self.recover_bound(&bound).await?;
                if inspection.health == BundleHealth::Invalid {
                    return Err(CompositionError::Unsettled);
                }
                inspection
            }
        };
        drop(bound);
        self.check_settled(&instance.record().instance.id)?;
        Ok(CompositionSettled {
            service: self.clone(),
            instance: instance.clone(),
            state: inspection.state,
        })
    }

    fn check_settled(&self, id: &InstanceId) -> Result<(), CompositionError> {
        if self
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(id.as_str())
        {
            return Err(CompositionError::Unsettled);
        }
        let busy = self.storage.read(|connection| {
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM performance_operations WHERE instance_id=?1)
                     OR EXISTS(SELECT 1 FROM performance_commands WHERE instance_id=?1 AND state IN ('queued','running'))",
                    [id.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(StorageError::from)
        })?;
        if busy {
            return Err(CompositionError::Unsettled);
        }
        Ok(())
    }

    /// Settles every recorded operation from evidence alone. Leaf recovery
    /// first settles the instance's own exact publication receipts; the
    /// recovered state then finishes the operation as applied when it is the
    /// recorded result or the verified expected composition, as interrupted
    /// when it is still the recorded prior state, and otherwise keeps it
    /// reserved and unresolved. No operation is planned or continued.
    pub async fn recover_pending(&self) -> Result<usize, CompositionError> {
        let mut settled = 0;
        let mut failure = None;
        for pending in self.pending()? {
            let admission = (|| {
                let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
                let retained = retained
                    .get_mut(pending.instance_id.as_str())
                    .ok_or(CompositionError::Unsettled)?;
                if retained.claimed {
                    return Ok(None);
                }
                retained.claimed = true;
                Ok(Some(retained.instance.clone()))
            })();
            let instance = match admission {
                Ok(Some(instance)) => instance,
                Ok(None) => continue,
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            };
            let result = async {
                instance
                    .directory()
                    .verify_receipt(&pending.directory_receipt)
                    .map_err(|_| CompositionError::Unsettled)?;
                let bound = self.bind(instance).await?;
                let inspection = self.recover_bound(&bound).await?;
                if inspection.health == BundleHealth::Invalid {
                    self.retain(bound.instance.clone(), Some(bound));
                    return Err(CompositionError::Unsettled);
                }
                // Validates the linked command record, when one exists.
                self.storage.read(|db| pending_command(db, &pending))?;
                if matches!(pending.expected, ExpectedComposition::Inspection) {
                    return self.finish(&pending, None);
                }
                let result = pending.result.as_ref().map(CompletedComposition::state);
                match classify_recovered(
                    pending.before.as_ref(),
                    &pending.expected,
                    result.as_ref(),
                    inspection.state.as_ref(),
                ) {
                    RestartDisposition::Applied => self.finish(&pending, None),
                    RestartDisposition::RestoredBefore => {
                        self.finish(&pending, Some(&CompositionError::Failed))
                    }
                    RestartDisposition::Preserve => {
                        self.retain(bound.instance.clone(), Some(bound));
                        Err(CompositionError::Unsettled)
                    }
                }
            }
            .await;
            match result {
                Ok(()) => settled += 1,
                Err(error) => {
                    self.unclaim(&pending.instance_id);
                    failure.get_or_insert(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(settled),
        }
    }

    fn unclaim(&self, id: &InstanceId) {
        if let Some(retained) = self
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id.as_str())
        {
            retained.claimed = false;
        }
    }

    async fn bind(&self, instance: RegisteredInstance) -> Result<BoundInstance, CompositionError> {
        let service = self.clone();
        axial_resource::process_physical_work()
            .admit(axial_resource::PhysicalWorkRequest::foreground(
                axial_resource::PhysicalIoClass::Metadata,
                0,
            ))
            .await
            .map_err(|_| CompositionError::Unsettled)?
            .run_parallel(move |_, workers| service.bind_admitted(instance, &workers))
            .await
            .map_err(|_| CompositionError::Unsettled)?
    }

    fn bind_admitted(
        &self,
        instance: RegisteredInstance,
        _workers: &axial_resource::PhysicalWorkParallelism,
    ) -> Result<BoundInstance, CompositionError> {
        instance
            .validate_current()
            .map_err(|_| CompositionError::InstanceUnavailable)?;
        if let Some(bound) = self
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(instance.record().instance.id.as_str())
            .and_then(|r| r.bound.clone())
        {
            return Ok(bound);
        }
        let (authority, identity) = ManagedCompositionAuthority::bind(
            instance.record().instance.id.as_str(),
            instance.directory().capability().clone(),
        )
        .map_err(|_| CompositionError::InstanceUnavailable)?;
        let effects = authority
            .bind_effects(&identity)
            .map_err(|_| CompositionError::Unsettled)?;
        Ok(BoundInstance {
            instance,
            authority,
            identity,
            effects,
        })
    }

    async fn recover_bound(
        &self,
        bound: &BoundInstance,
    ) -> Result<ManagedCompositionInspection, CompositionError> {
        let checkpoint = if !has_pending(&self.storage, &bound.instance.record().instance.id)? {
            let checkpoint = PendingOperation {
                operation_id: uuid::Uuid::new_v4().to_string(),
                instance_id: bound.instance.record().instance.id.clone(),
                directory_receipt: bound
                    .instance
                    .directory()
                    .receipt()
                    .map_err(|_| CompositionError::Unsettled)?,
                before: None,
                expected: ExpectedComposition::Inspection,
                target_effect_started: false,
                result: None,
            };
            self.begin(&checkpoint)?;
            self.retain(bound.instance.clone(), Some(bound.clone()));
            Some(checkpoint)
        } else {
            None
        };
        match bound
            .authority
            .recover_and_inspect(&bound.identity, &bound.effects)
            .await
        {
            Ok(inspection) => {
                if let Some(checkpoint) = checkpoint {
                    self.finish(&checkpoint, None)?;
                }
                Ok(inspection)
            }
            Err(_) => {
                self.retain(bound.instance.clone(), Some(bound.clone()));
                Err(CompositionError::Unsettled)
            }
        }
    }

    fn retain(&self, instance: RegisteredInstance, bound: Option<BoundInstance>) {
        self.retain_claimed(instance, bound, false);
    }

    fn retain_claimed(
        &self,
        instance: RegisteredInstance,
        bound: Option<BoundInstance>,
        claimed: bool,
    ) {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        let id = instance.record().instance.id.to_string();
        let claimed = claimed || retained.get(&id).is_some_and(|retained| retained.claimed);
        retained.insert(
            id,
            RetainedOperation {
                instance,
                bound,
                claimed,
            },
        );
    }
    fn pending(&self) -> Result<Vec<PendingOperation>, CompositionError> {
        self.storage.read(|connection| {
            let mut query = connection.prepare("SELECT instance_id,operation_id,payload FROM performance_operations ORDER BY instance_id LIMIT ?1")?;
            let mut rows = query.query([MAX_PENDING_OPERATIONS + 1])?;
            let mut pending = Vec::new();
            let mut total_bytes = 0usize;
            while let Some(row) = rows.next()? {
                let bytes = row.get_ref(2)?.as_blob().map_err(|_| CompositionError::Unsettled)?;
                if bytes.len() > MAX_PENDING_ROW_BYTES {
                    return Err(CompositionError::Unsettled);
                }
                total_bytes = total_bytes.checked_add(bytes.len()).ok_or(CompositionError::Unsettled)?;
                check_pending_budget(pending.len() + 1, total_bytes)?;
                let instance = row.get_ref(0)?.as_str().map_err(|_| CompositionError::Unsettled)?;
                let operation = row.get_ref(1)?.as_str().map_err(|_| CompositionError::Unsettled)?;
                pending.push(decode_pending(instance, operation, bytes)?);
            }
            Ok(pending)
        })
    }
    fn begin(&self, pending: &PendingOperation) -> Result<(), CompositionError> {
        let bytes = serde_json::to_vec(pending).map_err(|_| CompositionError::Unsettled)?;
        self.storage.transaction(|tx| {
            let (count, total_bytes) = pending_inventory(tx)?;
            check_pending_budget(count + 1, total_bytes.checked_add(bytes.len()).ok_or(CompositionError::Unsettled)?)?;
            let command = pending_command(tx, pending)?;
            if command.is_none() && !matches!(pending.expected, ExpectedComposition::Inspection) {
                return Err(CompositionError::Unsettled);
            }
            if tx.execute("INSERT INTO performance_operations(instance_id,operation_id,payload) VALUES(?1,?2,?3)", params![pending.instance_id.as_str(), pending.operation_id, bytes])? != 1
                || read_pending(tx, &pending.instance_id)?.as_ref() != Some(pending)
                || pending_command(tx, pending)? != command {
                return Err(CompositionError::Unsettled);
            }
            pending_inventory(tx)?;
            Ok(())
        })
    }
    fn finish(
        &self,
        pending: &PendingOperation,
        failure: Option<&CompositionError>,
    ) -> Result<(), CompositionError> {
        self.storage.transaction(|tx| {
            if read_pending(tx, &pending.instance_id)?.as_ref() != Some(pending) {
                return Err(CompositionError::Unsettled);
            }
            let command = pending_command(tx, pending)?.map(|mut status| {
                status.state = if failure.is_some() {
                    "failed"
                } else {
                    "complete"
                }
                .into();
                status.error = failure.map(ToString::to_string);
                status.updated_at = chrono::Utc::now().to_rfc3339();
                status
            });
            if let Some(status) = &command {
                write_command(tx, status)?;
            }
            if tx.execute(
                "DELETE FROM performance_operations WHERE instance_id=?1 AND operation_id=?2",
                params![pending.instance_id.as_str(), pending.operation_id],
            )? != 1
            {
                return Err(CompositionError::Unsettled);
            }
            if read_pending(tx, &pending.instance_id)?.is_some() {
                return Err(CompositionError::Unsettled);
            }
            if let Some(status) = &command {
                let saved: (String, String, Vec<u8>) = tx.query_row(
                    "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
                    [&status.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if command_status(&status.id, &saved.0, &saved.1, &saved.2)? != *status {
                    return Err(CompositionError::Unsettled);
                }
            }
            Ok(())
        })?;
        self.retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pending.instance_id.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn budget_checkpoint(service: &CompositionService) -> PendingOperation {
        use crate::instances::create::{CreateTarget, InstanceService, tests::request};
        let mut request = request(&format!("Pending budget {}", uuid::Uuid::new_v4()));
        request.selection_id = "fixture-fabric".into();
        let instances = InstanceService::new(service.instances.clone(), service.tasks.clone());
        let instance = instances
            .create(
                request,
                CreateTarget {
                    selection_id: "fixture-fabric".into(),
                    version_id: "fixture-fabric".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "fabric".into(),
                },
                instances.creation_admission_for_tests().await.unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let admitted = service.instances.admit(&instance.id).unwrap();
        PendingOperation {
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: instance.id,
            directory_receipt: admitted.directory().receipt().unwrap(),
            before: None,
            expected: ExpectedComposition::Inspection,
            target_effect_started: false,
            result: None,
        }
    }

    fn restore_budget_service(
        service: &CompositionService,
    ) -> Result<CompositionService, CompositionError> {
        CompositionService::new(
            service.storage.clone(),
            service.instances.clone(),
            service.tasks.clone(),
        )
    }

    fn padded_checkpoint(
        service: &CompositionService,
        pending: &PendingOperation,
        length: usize,
    ) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(pending).unwrap();
        assert!(bytes.len() <= length);
        bytes.resize(length, b' ');
        service
            .storage
            .transaction(|tx| {
                assert_eq!(
                    tx.execute(
                        "UPDATE performance_operations SET payload=?1 WHERE instance_id=?2",
                        params![bytes, pending.instance_id.as_str()]
                    )?,
                    1
                );
                Ok::<_, StorageError>(())
            })
            .unwrap();
        bytes
    }

    #[tokio::test]
    async fn pending_budget_count_is_inclusive_and_oversized_restore_is_preserved() {
        let (_root, service, admitted) = launch_fixture().await;
        drop(admitted);
        for _ in 0..MAX_PENDING_OPERATIONS {
            service.begin(&budget_checkpoint(&service).await).unwrap();
        }
        assert_eq!(service.pending().unwrap().len(), MAX_PENDING_OPERATIONS);
        let restored = restore_budget_service(&service).unwrap();
        assert_eq!(
            restored.retained.lock().unwrap().len(),
            MAX_PENDING_OPERATIONS
        );
        drop(restored);
        let extra = budget_checkpoint(&service).await;
        assert!(matches!(
            service.begin(&extra),
            Err(CompositionError::Unsettled)
        ));
        assert!(!has_pending(&service.storage, &extra.instance_id).unwrap());
        service
            .storage
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO performance_operations VALUES(?1,?2,?3)",
                    params![
                        extra.instance_id.as_str(),
                        extra.operation_id,
                        serde_json::to_vec(&extra).unwrap()
                    ],
                )?;
                Ok::<_, StorageError>(())
            })
            .unwrap();
        let before: Vec<(String, Vec<u8>)> = service.storage.read(|db| {
            Ok::<_, StorageError>(db.prepare("SELECT operation_id,payload FROM performance_operations ORDER BY instance_id")?.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?)
        }).unwrap();
        assert!(matches!(
            service.pending(),
            Err(CompositionError::Unsettled)
        ));
        assert!(matches!(
            restore_budget_service(&service),
            Err(CompositionError::Unsettled)
        ));
        let after: Vec<(String, Vec<u8>)> = service.storage.read(|db| {
            Ok::<_, StorageError>(db.prepare("SELECT operation_id,payload FROM performance_operations ORDER BY instance_id")?.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?)
        }).unwrap();
        assert_eq!(after, before);
        service.finish(&extra, None).unwrap();
        assert_eq!(service.pending().unwrap().len(), MAX_PENDING_OPERATIONS);
    }

    #[tokio::test]
    async fn pending_budget_bytes_are_inclusive_and_exact_settlement_can_reduce_overflow() {
        let (_root, service, admitted) = launch_fixture().await;
        drop(admitted);
        let mut fillers = Vec::new();
        for _ in 0..4 {
            let pending = budget_checkpoint(&service).await;
            service.begin(&pending).unwrap();
            fillers.push(pending);
        }
        let extra = budget_checkpoint(&service).await;
        let extra_bytes = serde_json::to_vec(&extra).unwrap();
        for filler in &fillers[..3] {
            padded_checkpoint(&service, filler, MAX_PENDING_ROW_BYTES);
        }
        padded_checkpoint(
            &service,
            &fillers[3],
            MAX_PENDING_ROW_BYTES - extra_bytes.len(),
        );
        service.begin(&extra).unwrap();
        assert_eq!(
            service.storage.read(pending_inventory).unwrap(),
            (5, MAX_PENDING_BYTES)
        );
        assert_eq!(service.pending().unwrap().len(), 5);
        service.finish(&extra, None).unwrap();
        padded_checkpoint(
            &service,
            &fillers[3],
            MAX_PENDING_ROW_BYTES - extra_bytes.len() + 1,
        );
        assert!(matches!(
            service.begin(&extra),
            Err(CompositionError::Unsettled)
        ));
        service
            .storage
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO performance_operations VALUES(?1,?2,?3)",
                    params![extra.instance_id.as_str(), extra.operation_id, extra_bytes],
                )?;
                Ok::<_, StorageError>(())
            })
            .unwrap();
        assert!(matches!(
            service.pending(),
            Err(CompositionError::Unsettled)
        ));
        assert!(matches!(
            restore_budget_service(&service),
            Err(CompositionError::Unsettled)
        ));
        assert_eq!(
            service
                .storage
                .read(|db| {
                    Ok::<_, StorageError>(db.query_row(
                        "SELECT sum(length(payload)) FROM performance_operations",
                        [],
                        |row| row.get::<_, usize>(0),
                    )?)
                })
                .unwrap(),
            MAX_PENDING_BYTES + 1
        );
        service.finish(&extra, None).unwrap();
        let restored = restore_budget_service(&service).unwrap();
        assert_eq!(restored.recover_pending().await.unwrap(), 4);
        assert_eq!(restored.pending_count().unwrap(), 0);
        assert!(!restored.has_unsettled_effects());
    }

    #[tokio::test]
    async fn pending_budget_rechecks_trigger_growth_before_acknowledging_admission() {
        let (_root, service, admitted) = launch_fixture().await;
        drop(admitted);
        let mut fillers = Vec::new();
        for _ in 0..4 {
            let pending = budget_checkpoint(&service).await;
            service.begin(&pending).unwrap();
            fillers.push(pending);
        }
        let extra = budget_checkpoint(&service).await;
        for filler in &fillers[..3] {
            padded_checkpoint(&service, filler, MAX_PENDING_ROW_BYTES);
        }
        let before = padded_checkpoint(
            &service,
            &fillers[3],
            MAX_PENDING_ROW_BYTES - serde_json::to_vec(&extra).unwrap().len(),
        );
        service.storage.transaction(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER inflate_pending AFTER INSERT ON performance_operations BEGIN UPDATE performance_operations SET payload=CAST(CAST(payload AS TEXT)||' ' AS BLOB) WHERE instance_id='{}'; END;", fillers[3].instance_id))?;
            Ok::<_, StorageError>(())
        }).unwrap();
        assert!(matches!(
            service.begin(&extra),
            Err(CompositionError::Unsettled)
        ));
        assert!(!has_pending(&service.storage, &extra.instance_id).unwrap());
        let after: Vec<u8> = service
            .storage
            .read(|db| {
                Ok::<_, StorageError>(db.query_row(
                    "SELECT payload FROM performance_operations WHERE instance_id=?1",
                    [fillers[3].instance_id.as_str()],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(after, before);
    }

    fn budget_command(instance: &InstanceId) -> PerformanceOperationStatus {
        PerformanceOperationStatus {
            id: uuid::Uuid::new_v4().to_string(),
            instance_id: instance.to_string(),
            action: "remove".into(),
            state: "queued".into(),
            error: None,
            created_at: "2026-09-28T00:00:00Z".into(),
            updated_at: "2026-09-28T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn pending_budget_active_admission_preserves_transitions_and_rolls_back_growth() {
        let (_root, service, admitted) = launch_fixture().await;
        let id = admitted.record().instance.id.clone();
        let mut commands: Vec<_> = (0..MAX_ACTIVE_COMMANDS)
            .map(|_| budget_command(&id))
            .collect();
        service
            .storage
            .transaction(|tx| {
                for command in &commands {
                    write_command(tx, command)?;
                }
                Ok::<_, CompositionError>(())
            })
            .unwrap();
        commands[0].state = "running".into();
        service
            .storage
            .transaction(|tx| write_command(tx, &commands[0]))
            .unwrap();
        let extra = budget_command(&id);
        assert!(matches!(
            service.storage.transaction(|tx| write_command(tx, &extra)),
            Err(CompositionError::Unsettled)
        ));
        assert!(service.operation(&extra.id).unwrap().is_none());
        commands[0].state = "unsettled".into();
        service
            .storage
            .transaction(|tx| write_command(tx, &commands[0]))
            .unwrap();
        service
            .storage
            .transaction(|tx| write_command(tx, &extra))
            .unwrap();
        commands[0].state = "running".into();
        assert!(matches!(
            service
                .storage
                .transaction(|tx| write_command(tx, &commands[0])),
            Err(CompositionError::Unsettled)
        ));
        assert_eq!(
            service.operation(&commands[0].id).unwrap().unwrap().state,
            "unsettled"
        );
        commands[1].state = "failed".into();
        service
            .storage
            .transaction(|tx| write_command(tx, &commands[1]))
            .unwrap();
        service
            .storage
            .transaction(|tx| write_command(tx, &commands[0]))
            .unwrap();
        commands[0].state = "complete".into();
        service
            .storage
            .transaction(|tx| write_command(tx, &commands[0]))
            .unwrap();
        let injected = budget_command(&id);
        let candidate = budget_command(&id);
        service.storage.transaction(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER inflate_active AFTER INSERT ON performance_commands WHEN NEW.id='{}' BEGIN INSERT INTO performance_commands VALUES('{}','{}','queued',CAST('{}' AS BLOB)); END;", candidate.id, injected.id, injected.instance_id, serde_json::to_string(&injected).unwrap()))?;
            Ok::<_, StorageError>(())
        }).unwrap();
        assert!(matches!(
            service
                .storage
                .transaction(|tx| write_command(tx, &candidate)),
            Err(CompositionError::Unsettled)
        ));
        assert!(service.operation(&candidate.id).unwrap().is_none());
        assert!(service.operation(&injected.id).unwrap().is_none());
        assert_eq!(
            service.storage.read(active_command_count).unwrap(),
            MAX_ACTIVE_COMMANDS - 1
        );
    }

    #[tokio::test]
    async fn pending_budget_active_restore_validates_the_whole_inventory_before_updates() {
        let (_root, service, admitted) = launch_fixture().await;
        let id = admitted.record().instance.id.clone();
        drop(admitted);
        let commands: Vec<_> = (0..=MAX_ACTIVE_COMMANDS)
            .map(|_| budget_command(&id))
            .collect();
        service
            .storage
            .transaction(|tx| {
                for command in &commands[..MAX_ACTIVE_COMMANDS] {
                    write_command(tx, command)?;
                }
                let extra = &commands[MAX_ACTIVE_COMMANDS];
                tx.execute(
                    "INSERT INTO performance_commands VALUES(?1,?2,?3,?4)",
                    params![
                        extra.id,
                        extra.instance_id,
                        extra.state,
                        serde_json::to_vec(extra).unwrap()
                    ],
                )?;
                Ok::<_, CompositionError>(())
            })
            .unwrap();
        assert!(matches!(
            restore_budget_service(&service),
            Err(CompositionError::Unsettled)
        ));
        for command in &commands {
            assert_eq!(
                service.operation(&command.id).unwrap().as_ref(),
                Some(command)
            );
        }
        let mut extra = commands[MAX_ACTIVE_COMMANDS].clone();
        extra.state = "failed".into();
        service
            .storage
            .transaction(|tx| write_command(tx, &extra))
            .unwrap();
        let restored = restore_budget_service(&service).unwrap();
        assert_eq!(service.storage.read(active_command_count).unwrap(), 0);
        for command in &commands[..MAX_ACTIVE_COMMANDS] {
            assert_eq!(
                restored.operation(&command.id).unwrap().unwrap().state,
                "interrupted"
            );
        }
        assert_eq!(
            restored.operation(&extra.id).unwrap().as_ref(),
            Some(&extra)
        );
    }

    #[test]
    fn pending_budget_indexes_active_reads_without_scanning_terminal_commands() {
        let storage = MetadataStore::in_memory_with_limits(crate::storage::StorageLimits {
            max_vm_instructions: 10_000,
            ..Default::default()
        })
        .unwrap();
        storage.migrate(&[MIGRATION]).unwrap();
        let command = budget_command(&InstanceId::new());
        storage
            .transaction(|tx| write_command(tx, &command))
            .unwrap();
        for _ in 0..64 {
            storage
                .transaction(|tx| {
                    for _ in 0..64 {
                        let mut terminal = budget_command(&InstanceId::new());
                        terminal.state = "complete".into();
                        write_command(tx, &terminal)?;
                    }
                    Ok::<_, CompositionError>(())
                })
                .unwrap();
        }
        assert_eq!(storage.read(active_command_count).unwrap(), 1);
        storage.read(|db| {
            let bytes: Vec<u8> = db.query_row("SELECT payload FROM performance_commands WHERE id=?1", [&command.id], |row| row.get(0))?;
            assert_eq!(bytes, serde_json::to_vec(&command).unwrap());
            for sql in [
                "EXPLAIN QUERY PLAN SELECT count(*) FROM (SELECT 1 FROM performance_commands WHERE state IN ('queued','running') LIMIT 129)",
                "EXPLAIN QUERY PLAN SELECT id,instance_id,state,payload FROM performance_commands WHERE state IN ('queued','running') LIMIT 129",
            ] {
                let plan: Vec<String> = db.prepare(sql)?.query_map([], |row| row.get(3))?.collect::<Result<_, _>>()?;
                assert!(plan.iter().any(|line| line.contains("performance_commands_active")), "{plan:?}");
            }
            Ok::<_, StorageError>(())
        }).unwrap();
    }

    fn latest_command(service: &CompositionService, id: &InstanceId) -> PerformanceOperationStatus {
        let operation = service
            .storage
            .read(|db| -> Result<String, StorageError> {
                Ok(db.query_row(
                    "SELECT id FROM performance_commands WHERE instance_id=?1 ORDER BY rowid DESC LIMIT 1",
                    [id.as_str()],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        service.operation(&operation).unwrap().unwrap()
    }

    /// Writes an operation and its running command exactly as an earlier run
    /// of this application recorded them, then reopens the owner over them.
    fn recorded(
        service: &CompositionService,
        admitted: RegisteredInstance,
        action: &str,
        before: Option<CompositionState>,
        expected: ExpectedComposition,
        result: Option<CompletedComposition>,
    ) -> (CompositionService, PendingOperation) {
        let now = chrono::Utc::now().to_rfc3339();
        let command = PerformanceOperationStatus {
            id: uuid::Uuid::new_v4().to_string(),
            instance_id: admitted.record().instance.id.to_string(),
            action: action.into(),
            state: "running".into(),
            error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        let pending = PendingOperation {
            operation_id: command.id.clone(),
            instance_id: admitted.record().instance.id.clone(),
            directory_receipt: admitted.directory().receipt().unwrap(),
            before,
            expected,
            target_effect_started: result.is_some(),
            result,
        };
        drop(admitted);
        service
            .storage
            .transaction(|tx| {
                write_command(tx, &command)?;
                tx.execute(
                    "INSERT INTO performance_operations(instance_id,operation_id,payload) VALUES(?1,?2,?3)",
                    params![
                        pending.instance_id.as_str(),
                        pending.operation_id,
                        serde_json::to_vec(&pending).unwrap()
                    ],
                )?;
                Ok::<_, CompositionError>(())
            })
            .unwrap();
        let reopened = CompositionService::new(
            service.storage.clone(),
            service.instances.clone(),
            service.tasks.clone(),
        )
        .unwrap();
        assert!(reopened.instances.admit(&pending.instance_id).is_err());
        (reopened, pending)
    }

    fn graph(composition_id: &str) -> ExpectedComposition {
        ExpectedComposition::Graph {
            composition_id: composition_id.into(),
            game_version: "1.21.4".into(),
            loader: "fabric".into(),
            graph_sha512: "0".repeat(128),
            artifacts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn recorded_unstarted_work_over_unchanged_files_finishes_interrupted_without_planning() {
        let (_root, service, admitted) = launch_fixture_with_storage(true).await;
        let before = super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        std::fs::write(path.join("mods/user.jar"), b"untouched user file").unwrap();
        let files = payload_files(&path);
        let id = admitted.record().instance.id.clone();
        drop(admitted);
        for (action, expected) in [
            ("apply", graph("replacement")),
            ("remove", ExpectedComposition::Absent),
        ] {
            let admitted = service.instances.admit(&id).unwrap();
            let (reopened, pending) = recorded(
                &service,
                admitted,
                action,
                Some(before.clone()),
                expected,
                None,
            );
            assert_eq!(reopened.recover_pending().await.unwrap(), 1);
            let command = reopened.operation(&pending.operation_id).unwrap().unwrap();
            assert_eq!(command.state, "failed", "{action}");
            assert_eq!(
                command.error.as_deref(),
                Some(CompositionError::Failed.to_string().as_str())
            );
            assert_eq!(
                payload_files(&path),
                files,
                "{action}: nothing was planned or started"
            );
            assert_eq!(reopened.pending_count().unwrap(), 0);
            assert!(!reopened.has_unsettled_effects());
            drop(reopened.instances.admit(&pending.instance_id).unwrap());
        }
    }

    #[tokio::test]
    async fn recorded_result_proven_by_recovered_files_finishes_applied() {
        let (_root, service, admitted) = launch_fixture_with_storage(true).await;
        let state = super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        let files = payload_files(&path);
        let (reopened, pending) = recorded(
            &service,
            admitted,
            "apply",
            None,
            graph("recorded"),
            Some(CompletedComposition::Managed(state)),
        );
        assert_eq!(reopened.recover_pending().await.unwrap(), 1);
        let command = reopened.operation(&pending.operation_id).unwrap().unwrap();
        assert_eq!((command.state.as_str(), command.error), ("complete", None));
        assert_eq!(payload_files(&path), files);
        assert_eq!(reopened.pending_count().unwrap(), 0);
        reopened.instances.admit(&pending.instance_id).unwrap();
    }

    #[tokio::test]
    async fn unexplained_recorded_state_stays_reserved_with_its_proof_and_files() {
        let (_root, service, admitted) = launch_fixture_with_storage(true).await;
        super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        let files = payload_files(&path);
        // The record says the target effect started toward absence and no
        // result was saved, yet a managed composition is present.
        let (reopened, pending) = recorded(
            &service,
            admitted,
            "remove",
            None,
            ExpectedComposition::Absent,
            None,
        );
        let mut pending = pending;
        pending.target_effect_started = true;
        reopened
            .storage
            .transaction(|tx| {
                tx.execute(
                    "UPDATE performance_operations SET payload=?1 WHERE instance_id=?2",
                    params![
                        serde_json::to_vec(&pending).unwrap(),
                        pending.instance_id.as_str()
                    ],
                )?;
                Ok::<_, StorageError>(())
            })
            .unwrap();
        let recorded_payload = serde_json::to_vec(&pending).unwrap();
        for attempt in 0..2 {
            assert!(
                matches!(
                    reopened.recover_pending().await,
                    Err(CompositionError::Unsettled)
                ),
                "attempt {attempt}"
            );
            assert_eq!(reopened.pending_count().unwrap(), 1);
            assert!(reopened.has_unsettled_effects());
            assert!(reopened.instances.admit(&pending.instance_id).is_err());
            assert_eq!(payload_files(&path), files);
            let saved: Vec<u8> = reopened
                .storage
                .read(|db| {
                    Ok::<_, StorageError>(db.query_row(
                        "SELECT payload FROM performance_operations WHERE instance_id=?1",
                        [pending.instance_id.as_str()],
                        |row| row.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(saved, recorded_payload);
            assert_eq!(
                latest_command(&reopened, &pending.instance_id).state,
                "running"
            );
        }
    }

    async fn launch_fixture() -> (tempfile::TempDir, CompositionService, RegisteredInstance) {
        launch_fixture_with_storage(false).await
    }

    async fn launch_fixture_with_storage(
        durable: bool,
    ) -> (tempfile::TempDir, CompositionService, RegisteredInstance) {
        use crate::{
            instances::{
                create::{CreateTarget, InstanceService, tests::request},
                directory::Registry,
            },
            library::{LibraryLifecycle, LibraryOpenOutcome},
            tasks::Exclusions,
        };
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("fixture library did not open: {other:?}"),
        };
        let storage = Arc::new(if durable {
            MetadataStore::open(root.path().join("metadata.sqlite")).unwrap()
        } else {
            MetadataStore::in_memory().unwrap()
        });
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::content::install::MIGRATION,
                MIGRATION,
                super::super::RULES_MIGRATION,
            ])
            .unwrap();
        let tasks = TaskOwner::new(8).unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage.clone()), library, Exclusions::new());
        let instances = InstanceService::new(directories.clone(), tasks.clone());
        let mut request = request("Performance launch");
        request.selection_id = "fixture-fabric".into();
        let instance = instances
            .create(
                request,
                CreateTarget {
                    selection_id: "fixture-fabric".into(),
                    version_id: "fixture-fabric".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "fabric".into(),
                },
                instances.creation_admission_for_tests().await.unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let service = CompositionService::new(storage.clone(), directories.clone(), tasks).unwrap();
        let admitted = directories.admit(&instance.id).unwrap();
        (root, service, admitted)
    }

    fn payload_files(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut directories = vec![root.to_path_buf()];
        let mut files = BTreeMap::new();
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    directories.push(entry.path());
                } else {
                    files.insert(
                        entry.path().strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
        files
    }

    #[tokio::test]
    async fn launch_proves_absent_composition_without_writes_and_preserves_user_mods() {
        let (_root, service, admitted) = launch_fixture().await;
        let path = admitted.directory().read_projection().unwrap();
        std::fs::write(path.join("mods/sodium-fabric.jar"), b"user-selected Sodium").unwrap();
        let before = payload_files(&path);
        let settled = service.settled_for_launch(&admitted).await.unwrap();
        assert!(settled.state.is_none());
        settled.revalidate().await.unwrap();
        assert_eq!(payload_files(&path), before);
        assert!(!path.join("mods/.axial-performance").exists());
        assert_eq!(service.pending_count().unwrap(), 0);
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn launch_proves_settled_managed_artifacts_and_history_without_writes() {
        let (_root, service, admitted) = launch_fixture().await;
        super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        let before = payload_files(&path);
        assert!(before.contains_key(std::path::Path::new("mods/current.jar")));
        assert!(before.keys().any(|path| path.ends_with("snapshot.json")));
        let settled = service.settled_for_launch(&admitted).await.unwrap();
        assert!(settled.state.is_some());
        settled.revalidate().await.unwrap();
        assert_eq!(payload_files(&path), before);
        assert_eq!(service.pending_count().unwrap(), 0);
        assert!(!service.has_unsettled_effects());

        // A managed artifact changed after the proof fails the spawn recheck.
        let original = std::fs::read(path.join("mods/current.jar")).unwrap();
        std::fs::write(path.join("mods/current.jar"), b"changed before spawn").unwrap();
        assert!(matches!(
            settled.revalidate().await,
            Err(CompositionError::Unsettled)
        ));
        std::fs::write(path.join("mods/current.jar"), original).unwrap();
        settled.revalidate().await.unwrap();
    }

    #[tokio::test]
    async fn launch_refuses_changed_managed_artifacts_and_retains_exact_settlement() {
        let (_root, service, admitted) = launch_fixture().await;
        super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        let original = std::fs::read(path.join("mods/current.jar")).unwrap();
        std::fs::write(path.join("mods/current.jar"), b"externally changed").unwrap();
        let before = payload_files(&path);
        let id = admitted.record().instance.id.clone();
        for _ in 0..2 {
            assert!(matches!(
                service.settled_for_launch(&admitted).await,
                Err(CompositionError::Unsettled)
            ));
            assert_eq!(payload_files(&path), before);
            assert_eq!(service.pending_count().unwrap(), 1);
            assert!(service.has_unsettled_effects());
        }
        drop(admitted);
        assert!(matches!(
            service.instances.admit(&id),
            Err(crate::instances::model::InstanceError::Busy)
        ));
        // The fixture restores the exact bytes it deliberately changed, then
        // settles through the owner before dropping its native root lifetime.
        std::fs::write(path.join("mods/current.jar"), original).unwrap();
        assert_eq!(service.recover_pending().await.unwrap(), 1);
        assert_eq!(service.pending_count().unwrap(), 0);
        assert!(!service.has_unsettled_effects());
        service
            .instances
            .admit(&id)
            .unwrap()
            .validate_current()
            .unwrap();
    }

    #[test]
    fn mutation_diagnostics_distinguish_unsettled_from_preserved_failure_without_inner_payloads() {
        let private = "private path and remote response must remain private";
        assert_eq!(
            CompositionError::Storage(StorageError::InvalidMigration(private.into()))
                .diagnostic_code(),
            "storage_unavailable"
        );
        assert_eq!(
            CompositionError::Failed.diagnostic_code(),
            "failed_preserved"
        );
        assert_eq!(CompositionError::Unsettled.diagnostic_code(), "unsettled");
    }

    #[test]
    fn completed_absence_survives_restart_as_positive_rollback_evidence() {
        let record = PendingOperation {
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: InstanceId::new(),
            directory_receipt: "exact-receipt".into(),
            before: None,
            expected: ExpectedComposition::Snapshot {
                snapshot_id: "snapshot".into(),
                prepared: None,
            },
            target_effect_started: true,
            result: Some(CompletedComposition::Absent),
        };
        let encoded = serde_json::to_vec(&record).unwrap();
        let loaded: PendingOperation = serde_json::from_slice(&encoded).unwrap();
        let result = loaded.result.as_ref().map(CompletedComposition::state);
        assert_eq!(
            classify_recovered(None, &loaded.expected, result.as_ref(), None),
            RestartDisposition::Applied
        );
        assert_eq!(
            classify_recovered(None, &loaded.expected, None, None),
            RestartDisposition::RestoredBefore
        );
    }
}
