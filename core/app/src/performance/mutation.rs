//! Accepted Performance work owns its registered instance until exact settlement.

use super::{
    plan::{PreparedManagedPlan, prepare_managed_install},
    rollback::{ExpectedComposition, RestartDisposition, classify_recovered, select_snapshot},
    rules::{PerformanceRules, PlannedPerformance, RulesWorkflowError},
};
use crate::{
    content::catalog::ContentService,
    instances::{
        directory::{InstanceDirectories, RegisteredInstance},
        model::{InstanceError, InstanceId, InstanceLifecycle},
    },
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{self, Connection, OptionalExtension, Transaction, params},
    },
    tasks::{CancellationToken, TaskHandle, TaskOwner},
};
use axial_performance::{
    CompositionPlan, CompositionState, ManagedArtifactTransferResolver,
    ManagedCompositionAuthority, ManagedCompositionInspection, ManagedInstanceEffectAuthority,
    ManagedInstanceIdentity, ManagedRollbackOutcome, PerformanceMode, ResolutionRequest,
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
pub enum PerformanceMutationError {
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

impl PerformanceMutationError {
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

impl From<rusqlite::Error> for PerformanceMutationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<RulesWorkflowError> for PerformanceMutationError {
    fn from(_: RulesWorkflowError) -> Self {
        Self::PlanUnavailable
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
    fn from_state(state: Option<CompositionState>) -> Self {
        match state {
            Some(state) => Self::Managed(state),
            None => Self::Absent,
        }
    }
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

struct PreparedContinuation {
    pending: PendingOperation,
    command: PerformanceOperationStatus,
    bound: BoundInstance,
    inspection: ManagedCompositionInspection,
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
) -> Result<PerformanceOperationStatus, PerformanceMutationError> {
    let status: PerformanceOperationStatus =
        serde_json::from_slice(bytes).map_err(|_| PerformanceMutationError::Unsettled)?;
    validate_command(&status)?;
    if status.id != id || status.instance_id != instance || status.state != state {
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(status)
}

fn pending_command(
    connection: &Connection,
    pending: &PendingOperation,
) -> Result<Option<PerformanceOperationStatus>, PerformanceMutationError> {
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
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(Some(status))
}

fn prepared_command(pending: &PendingOperation, command: &PerformanceOperationStatus) -> bool {
    command.state == "running"
        && !pending.target_effect_started
        && pending.result.is_none()
        && matches!(
            pending.expected,
            ExpectedComposition::Graph { .. }
                | ExpectedComposition::Absent
                | ExpectedComposition::Snapshot {
                    prepared: Some(_),
                    ..
                }
        )
}

fn read_pending(
    connection: &Connection,
    instance: &InstanceId,
) -> Result<Option<PendingOperation>, PerformanceMutationError> {
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
) -> Result<PendingOperation, PerformanceMutationError> {
    let pending: PendingOperation =
        serde_json::from_slice(bytes).map_err(|_| PerformanceMutationError::Unsettled)?;
    if pending.instance_id.as_str() != instance
        || pending.operation_id != id
        || uuid::Uuid::parse_str(id).is_err()
    {
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(pending)
}

fn check_pending_budget(count: usize, bytes: usize) -> Result<(), PerformanceMutationError> {
    if count > MAX_PENDING_OPERATIONS || bytes > MAX_PENDING_BYTES {
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(())
}

fn pending_inventory(connection: &Connection) -> Result<(usize, usize), PerformanceMutationError> {
    let mut query =
        connection.prepare("SELECT length(payload) FROM performance_operations LIMIT ?1")?;
    let mut rows = query.query([MAX_PENDING_OPERATIONS + 1])?;
    let (mut count, mut bytes) = (0, 0usize);
    while let Some(row) = rows.next()? {
        let length: usize = row.get(0)?;
        if length > MAX_PENDING_ROW_BYTES {
            return Err(PerformanceMutationError::Unsettled);
        }
        count += 1;
        bytes = bytes
            .checked_add(length)
            .ok_or(PerformanceMutationError::Unsettled)?;
        check_pending_budget(count, bytes)?;
    }
    Ok((count, bytes))
}

fn validate_command(status: &PerformanceOperationStatus) -> Result<(), PerformanceMutationError> {
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
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(())
}

fn write_command(
    tx: &Transaction<'_>,
    status: &PerformanceOperationStatus,
) -> Result<(), PerformanceMutationError> {
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
        return Err(PerformanceMutationError::Unsettled);
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
            return Err(PerformanceMutationError::Unsettled);
        }
    }
    let bytes = serde_json::to_vec(status).map_err(|_| PerformanceMutationError::Unsettled)?;
    if tx.execute("INSERT INTO performance_commands(id,instance_id,state,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET state=excluded.state,payload=excluded.payload", params![status.id, status.instance_id, status.state, bytes])? != 1 {
        return Err(PerformanceMutationError::Unsettled);
    }
    let saved: (String, String, Vec<u8>) = tx.query_row(
        "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
        [&status.id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if saved != (status.instance_id.clone(), status.state.clone(), bytes) {
        return Err(PerformanceMutationError::Unsettled);
    }
    if admits_active && active_command_count(tx)? > MAX_ACTIVE_COMMANDS {
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(())
}

fn active_command_count(connection: &Connection) -> Result<usize, PerformanceMutationError> {
    Ok(connection.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM performance_commands WHERE state IN ('queued','running') LIMIT ?1)",
        [MAX_ACTIVE_COMMANDS + 1],
        |row| row.get(0),
    )?)
}

#[cfg(any(test, feature = "test-support"))]
fn crash_checkpoint_for_test(phase: &str) {
    let requested = std::env::var("AXIAL_PERFORMANCE_PREPARED_CRASH").ok();
    if requested.as_deref() == Some(phase)
        || (requested.as_deref() == Some("1") && phase == "prepared")
    {
        std::process::exit(if phase == "planning" {
            41
        } else if phase == "effect" {
            43
        } else {
            42
        });
    }
}

#[derive(Clone)]
pub struct PerformanceService {
    storage: Arc<MetadataStore>,
    instances: InstanceDirectories,
    tasks: TaskOwner,
    content: Arc<ContentService>,
    transfers: ManagedArtifactTransferResolver,
    rules: PerformanceRules,
    retained: Arc<Mutex<BTreeMap<String, RetainedOperation>>>,
}

pub struct PreparedPerformance {
    planned: Arc<PlannedPerformance>,
    instance: RegisteredInstance,
}

impl PreparedPerformance {
    pub fn plan(&self) -> &CompositionPlan {
        self.planned.plan()
    }
    pub fn effective(&self) -> axial_performance::EffectivePerformancePlan {
        axial_performance::effective_performance_plan(self.plan())
    }
    pub fn validate_current(&self) -> Result<(), PerformanceMutationError> {
        self.instance
            .validate_current()
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        self.planned.ensure_current()?;
        Ok(())
    }
}

impl PerformanceService {
    pub fn new(
        storage: Arc<MetadataStore>,
        instances: InstanceDirectories,
        tasks: TaskOwner,
        content: Arc<ContentService>,
        transfers: ManagedArtifactTransferResolver,
    ) -> Result<Self, PerformanceMutationError> {
        let rules = PerformanceRules::new(storage.clone())?;
        let service = Self {
            storage,
            instances,
            tasks,
            content,
            transfers,
            rules,
            retained: Arc::new(Mutex::new(BTreeMap::new())),
        };
        // Reserve all interrupted targets before other features admit work.
        // Corrupt records fail startup instead of silently releasing exclusion.
        for pending in service.pending()? {
            service.storage.read(|db| pending_command(db, &pending))?;
            let instance = service
                .instances
                .admit_for_performance_settlement(&pending.instance_id)
                .map_err(|_| PerformanceMutationError::Unsettled)?;
            instance
                .directory()
                .verify_receipt(&pending.directory_receipt)
                .map_err(|_| PerformanceMutationError::Unsettled)?;
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
                    return Err(PerformanceMutationError::Unsettled);
                }
                let id = row.get_ref(0)?.as_str().map_err(|_| PerformanceMutationError::Unsettled)?;
                let instance = row.get_ref(1)?.as_str().map_err(|_| PerformanceMutationError::Unsettled)?;
                let state = row.get_ref(2)?.as_str().map_err(|_| PerformanceMutationError::Unsettled)?;
                let bytes = row.get_ref(3)?.as_blob().map_err(|_| PerformanceMutationError::Unsettled)?;
                records.push(command_status(id, instance, state, bytes)?);
            }
            drop(rows);
            drop(query);
            for mut status in records {
                let instance_id = status.instance_id.parse().map_err(|_| PerformanceMutationError::Unsettled)?;
                if read_pending(tx, &instance_id)?.is_some_and(|pending| pending.operation_id == status.id) {
                    continue;
                }
                status.state = "interrupted".into();
                status.error =
                    Some("Operation interrupted; inspect the instance before retrying".into());
                write_command(tx, &status)?;
            }
            Ok::<_, PerformanceMutationError>(())
        })?;
        Ok(service)
    }

    pub fn rules(&self) -> &PerformanceRules {
        &self.rules
    }

    #[cfg(feature = "test-support")]
    pub fn with_rules_for_test(mut self, rules: PerformanceRules) -> Self {
        assert!(rules.uses_metadata(&self.storage));
        self.rules = rules;
        self
    }

    #[cfg(feature = "test-support")]
    pub async fn install_plan_for_test(
        &self,
        id: &InstanceId,
        plan: axial_performance::ManagedCompositionInstallPlan,
        transfers: ManagedArtifactTransferResolver,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        validate_target(
            &instance,
            &self.resolution_request(
                plan.game_version().into(),
                plan.loader().into(),
                PerformanceMode::Managed,
            ),
        )?;
        let service = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |_| async move {
                let bound = service.bind(instance).await?;
                let admitted = bound.instance.clone();
                bound
                    .authority
                    .ensure_installed(
                        &bound.identity,
                        &bound.effects,
                        &plan,
                        transfers,
                        move || async move { admitted.validate_current() },
                    )
                    .await
                    .map_err(|_| PerformanceMutationError::Failed)?;
                service.recover_bound(&bound).await
            })
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
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
    pub fn pending_count(&self) -> Result<usize, PerformanceMutationError> {
        Ok(self.pending()?.len())
    }

    /// Read-only ownership evidence under the consumer's existing admission.
    /// Empty is authoritative only after a successfully admitted absent state;
    /// corrupt metadata and missing authority return an error, never an empty set.
    pub async fn managed_witness_proofs(
        &self,
        instance: &RegisteredInstance,
    ) -> Result<Vec<axial_performance::ManagedArtifactWitnessProof>, PerformanceMutationError> {
        let instance = instance.clone();
        let service = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |_| async move {
                let bound = service.bind(instance).await?;
                bound
                    .authority
                    .composition_managed_witness_proofs(&bound.identity, &bound.effects)
                    .await
                    .map_err(|_| PerformanceMutationError::Unsettled)
            })
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }
    pub fn operation(
        &self,
        id: &str,
    ) -> Result<Option<PerformanceOperationStatus>, PerformanceMutationError> {
        self.storage.read(|connection| {
            let record = bounded_command_row(connection, id)?;
            record
                .map(|(instance, state, bytes)| command_status(id, &instance, &state, &bytes))
                .transpose()
        })
    }
    pub fn instance_operation(
        &self,
        id: &InstanceId,
    ) -> Result<Option<PerformanceOperationStatus>, PerformanceMutationError> {
        self.storage.read(|connection| {
            let instance = crate::instances::directory::get_in(connection, id).map_err(|error| match error {
                InstanceError::NotFound => PerformanceMutationError::InstanceNotFound,
                InstanceError::Storage(error) => PerformanceMutationError::Storage(error),
                _ => PerformanceMutationError::Storage(StorageError::Corrupt),
            })?;
            if instance.lifecycle != InstanceLifecycle::Live {
                return Err(PerformanceMutationError::InstanceUnavailable);
            }
            let record: Option<(String, String, Vec<u8>)> = connection.query_row("SELECT id,state,payload FROM performance_commands WHERE instance_id=?1 ORDER BY rowid DESC LIMIT 1", [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
            record.map(|(operation, state, bytes)| command_status(&operation, id.as_str(), &state, &bytes)).transpose()
        })
    }

    pub fn submit(
        &self,
        id: &InstanceId,
        request: ResolutionRequest,
        action: &str,
        snapshot: Option<String>,
    ) -> Result<PerformanceOperationStatus, PerformanceMutationError> {
        self.start_command(id, Some(request), action, snapshot)
            .map(|(status, _)| status)
    }

    fn start_command(
        &self,
        id: &InstanceId,
        request: Option<ResolutionRequest>,
        action: &str,
        snapshot: Option<String>,
    ) -> Result<
        (
            PerformanceOperationStatus,
            TaskHandle<Result<ManagedCompositionInspection, PerformanceMutationError>>,
        ),
        PerformanceMutationError,
    > {
        if !matches!(action, "apply" | "reapply" | "remove" | "rollback") {
            return Err(PerformanceMutationError::PlanUnavailable);
        }
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        if matches!(action, "apply" | "reapply") {
            validate_target(
                &instance,
                request
                    .as_ref()
                    .ok_or(PerformanceMutationError::PlanUnavailable)?,
            )?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        let status = PerformanceOperationStatus {
            id: uuid::Uuid::new_v4().to_string(),
            instance_id: id.to_string(),
            action: action.into(),
            state: "queued".into(),
            error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.save_operation(&status)?;
        let service = self.clone();
        let running = status.clone();
        let target = id.clone();
        let accepted = self
            .tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                let mut running = running;
                running.state = "running".into();
                service.save_operation(&running)?;
                #[cfg(any(test, feature = "test-support"))]
                crash_checkpoint_for_test("planning");
                let result = async {
                    let bound = service.bind(instance).await?;
                    let inspection = service.recover_bound(&bound).await?;
                    let action = match running.action.as_str() {
                        "apply" | "reapply" => {
                            service
                                .prepare_apply(
                                    &bound.instance,
                                    &inspection,
                                    request.ok_or(PerformanceMutationError::PlanUnavailable)?,
                                )
                                .await?
                        }
                        "rollback" => Action::Rollback(
                            select_snapshot(&inspection.rollback_snapshots, snapshot.as_deref())?
                                .clone(),
                        ),
                        "remove" => Action::Remove,
                        _ => return Err(PerformanceMutationError::Unsettled),
                    };
                    service
                        .execute(bound, inspection, action, cancel, &running.id, None)
                        .await
                }
                .await;
                let completed = service.complete_command(&running, &result);
                service.unclaim(&target);
                completed?;
                result
            });
        match accepted {
            Ok(task) => Ok((status, task)),
            Err(_) => {
                let mut failed = status;
                failed.state = "failed".into();
                failed.error = Some("Task owner refused operation".into());
                self.save_operation(&failed)?;
                Err(PerformanceMutationError::InstanceUnavailable)
            }
        }
    }

    fn save_operation(
        &self,
        status: &PerformanceOperationStatus,
    ) -> Result<(), PerformanceMutationError> {
        let mut status = status.clone();
        status.updated_at = chrono::Utc::now().to_rfc3339();
        self.storage.transaction(|tx| {
            write_command(tx, &status)?;
            tx.execute("DELETE FROM performance_commands WHERE state IN ('complete','failed') AND id NOT IN (SELECT operation_id FROM performance_operations) AND rowid NOT IN (SELECT rowid FROM performance_commands ORDER BY rowid DESC LIMIT 128)", [])?;
            Ok(())
        })
    }

    fn complete_command(
        &self,
        command: &PerformanceOperationStatus,
        result: &Result<ManagedCompositionInspection, PerformanceMutationError>,
    ) -> Result<(), PerformanceMutationError> {
        let current = self
            .operation(&command.id)?
            .ok_or(PerformanceMutationError::Unsettled)?;
        if matches!(current.state.as_str(), "complete" | "failed") {
            return Ok(());
        }
        let mut status = command.clone();
        let id = command
            .instance_id
            .parse()
            .map_err(|_| PerformanceMutationError::Unsettled)?;
        let pending = self.storage.read(|db| read_pending(db, &id))?;
        status.state = if pending.is_some() {
            "unsettled"
        } else if result.is_ok() {
            "complete"
        } else {
            "failed"
        }
        .into();
        status.error = result.as_ref().err().map(ToString::to_string);
        self.save_operation(&status)?;
        Ok(())
    }
    pub fn resolution_request(
        &self,
        game_version: String,
        loader: String,
        mode: PerformanceMode,
    ) -> ResolutionRequest {
        ResolutionRequest {
            game_version,
            loader,
            mode,
            hardware: self.rules.manager().hardware(),
            installed_mods: Vec::new(),
        }
    }

    pub async fn inspect(
        &self,
        id: &InstanceId,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.inspect_owned(id, None)
            .await
            .map(|(inspection, _)| inspection)
    }

    pub async fn resolve_and_inspect(
        &self,
        id: &InstanceId,
        request: ResolutionRequest,
    ) -> Result<axial_performance::ManagedResolvedInspection, PerformanceMutationError> {
        let (inspection, plan) = self.inspect_owned(id, Some(request)).await?;
        Ok(axial_performance::ManagedResolvedInspection {
            inspection,
            plan: plan.ok_or(PerformanceMutationError::PlanUnavailable)?,
        })
    }

    async fn inspect_owned(
        &self,
        id: &InstanceId,
        request: Option<ResolutionRequest>,
    ) -> Result<(ManagedCompositionInspection, Option<CompositionPlan>), PerformanceMutationError>
    {
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        let service = self.clone();
        let retained = instance.clone();
        self.tasks
            .try_spawn(retained, move |_| async move {
                let bound = service.bind(instance).await?;
                let admission = bound.instance.clone();
                let checkpoint = PendingOperation {
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    instance_id: admission.record().instance.id.clone(),
                    directory_receipt: admission
                        .directory()
                        .receipt()
                        .map_err(|_| PerformanceMutationError::Unsettled)?,
                    before: None,
                    expected: ExpectedComposition::Inspection,
                    target_effect_started: false,
                    result: None,
                };
                let checkpoint_for_admission = checkpoint.clone();
                let checkpoint_service = service.clone();
                let checkpoint_bound = bound.clone();
                let admit = move || {
                    admission.validate_current().map_err(|_| {
                        axial_performance::ManagedMutationError::reconciliation_required(
                            "instance_changed",
                        )
                    })?;
                    checkpoint_service
                        .begin(&checkpoint_for_admission)
                        .map_err(|_| {
                            axial_performance::ManagedMutationError::reconciliation_required(
                                "inspection_checkpoint",
                            )
                        })?;
                    checkpoint_service.retain(admission.clone(), Some(checkpoint_bound));
                    Ok(admission)
                };
                let inspection = match request {
                    Some(request) => {
                        // Keep validated rules pinned while the leaf replaces
                        // caller evidence with this admitted instance's files.
                        let _rules = service.rules.plan(request.clone()).await?;
                        bound
                            .authority
                            .resolve_and_inspect(&bound.identity, &bound.effects, request, admit)
                            .await
                            .map(|resolved| (resolved.inspection, Some(resolved.plan)))
                    }
                    None => bound
                        .authority
                        .inspect(&bound.identity, &bound.effects, None, admit)
                        .await
                        .map(|inspection| (inspection, None)),
                };
                match inspection {
                    Ok(inspection) => {
                        if service
                            .storage
                            .read(|db| read_pending(db, &checkpoint.instance_id))?
                            .is_some_and(|pending| pending.operation_id == checkpoint.operation_id)
                        {
                            service.finish(&checkpoint, None)?;
                        }
                        Ok(inspection)
                    }
                    Err(_) => {
                        if !has_pending(&service.storage, &checkpoint.instance_id)? {
                            service.begin(&checkpoint)?;
                        }
                        service.retain(bound.instance.clone(), Some(bound));
                        Err(PerformanceMutationError::Unsettled)
                    }
                }
            })
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    pub async fn apply(
        &self,
        id: &InstanceId,
        request: ResolutionRequest,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.start_command(id, Some(request), "apply", None)?
            .1
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    pub async fn reapply(
        &self,
        id: &InstanceId,
        request: ResolutionRequest,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.start_command(id, Some(request), "reapply", None)?
            .1
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    async fn prepare_apply(
        &self,
        instance: &RegisteredInstance,
        inspection: &ManagedCompositionInspection,
        mut request: ResolutionRequest,
    ) -> Result<Action, PerformanceMutationError> {
        validate_target(instance, &request)?;
        request.installed_mods = inspection.installed_mod_evidence.clone();
        let planned = Arc::new(self.rules.plan(request).await?);
        if planned.plan().mode == PerformanceMode::Managed {
            Ok(Action::Apply(
                prepare_managed_install(&self.content, planned)
                    .await
                    .map_err(|_| PerformanceMutationError::PlanUnavailable)?,
            ))
        } else {
            Ok(Action::Remove)
        }
    }

    pub async fn remove(
        &self,
        id: &InstanceId,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.start_command(id, None, "remove", None)?
            .1
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    pub async fn rollback(
        &self,
        id: &InstanceId,
        snapshot: Option<String>,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.start_command(id, None, "rollback", snapshot)?
            .1
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    /// Settle existing obligations and pin the selected mode's launch plan under
    /// launch's admission. Applying or removing bundles requires an explicit
    /// Performance command; Play does not authorize new content mutations.
    pub async fn prepare_for_launch(
        &self,
        instance: &RegisteredInstance,
        request: ResolutionRequest,
    ) -> Result<PreparedPerformance, PerformanceMutationError> {
        validate_target(instance, &request)?;
        let service = self.clone();
        let instance = instance.clone();
        self.tasks
            .try_spawn(instance.clone(), move |_| async move {
                let bound = service.bind(instance.clone()).await?;
                let inspection = service.recover_bound(&bound).await?;
                if inspection.health == axial_performance::BundleHealth::Invalid {
                    return Err(PerformanceMutationError::PlanUnavailable);
                }
                let mut request = request;
                request.installed_mods = inspection.installed_mod_evidence.clone();
                let planned = Arc::new(service.rules.plan(request).await?);
                let prepared = PreparedPerformance { planned, instance };
                prepared.validate_current()?;
                Ok(prepared)
            })
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    /// Explicit restart settlement; no Guardian policy or discretionary repair.
    pub async fn recover_pending(&self) -> Result<usize, PerformanceMutationError> {
        let mut settled = 0;
        let mut prepared = Vec::new();
        let mut failure = None;
        for pending in self.pending()? {
            let admission = (|| {
                let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
                let retained = retained
                    .get_mut(pending.instance_id.as_str())
                    .ok_or(PerformanceMutationError::Unsettled)?;
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
                    if failure.is_none() {
                        failure = Some(error);
                    }
                    continue;
                }
            };
            let result = async {
                instance
                    .directory()
                    .verify_receipt(&pending.directory_receipt)
                    .map_err(|_| PerformanceMutationError::Unsettled)?;
                let bound = self.bind(instance).await?;
                let inspection = self.recover_bound(&bound).await?;
                if inspection.health == axial_performance::BundleHealth::Invalid {
                    return Err(PerformanceMutationError::Unsettled);
                }
                let command = self.storage.read(|db| pending_command(db, &pending))?;
                if let Some(command) = command.filter(|command| prepared_command(&pending, command))
                {
                    self.retain(bound.instance.clone(), Some(bound.clone()));
                    return Ok(Some(PreparedContinuation {
                        pending: pending.clone(),
                        command,
                        bound,
                        inspection,
                    }));
                }
                if matches!(pending.expected, ExpectedComposition::Inspection) {
                    self.finish(&pending, None)?;
                    return Ok(None);
                }
                let result = pending.result.as_ref().map(CompletedComposition::state);
                match classify_recovered(
                    pending.before.as_ref(),
                    &pending.expected,
                    result.as_ref(),
                    inspection.state.as_ref(),
                ) {
                    RestartDisposition::Applied => self.finish(&pending, None)?,
                    RestartDisposition::RestoredBefore => {
                        self.finish(&pending, Some(&PerformanceMutationError::Failed))?
                    }
                    RestartDisposition::Preserve => {
                        self.retain(bound.instance.clone(), Some(bound));
                        return Err(PerformanceMutationError::Unsettled);
                    }
                }
                Ok(None)
            }
            .await;
            match result {
                Ok(Some(continuation)) => prepared.push(continuation),
                Ok(None) => settled += 1,
                Err(error) => {
                    self.unclaim(&pending.instance_id);
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        if !prepared.is_empty() {
            let ids = prepared
                .iter()
                .map(|item| item.pending.instance_id.clone())
                .collect::<Vec<_>>();
            let count = prepared.len();
            let service = self.clone();
            if self
                .tasks
                .try_spawn(self.clone(), move |cancel| async move {
                    for continuation in prepared {
                        let result = service.resume_prepared(&continuation, cancel.clone()).await;
                        let _ = service.complete_command(&continuation.command, &result);
                        service.unclaim(&continuation.pending.instance_id);
                    }
                })
                .is_err()
            {
                for id in ids {
                    self.unclaim(&id);
                }
                return Err(PerformanceMutationError::InstanceUnavailable);
            }
            settled += count;
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(settled),
        }
    }

    async fn resume_prepared(
        &self,
        continuation: &PreparedContinuation,
        cancel: CancellationToken,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        let pending = &continuation.pending;
        let bound = &continuation.bound;
        let result = async {
            if cancel.is_cancelled() {
                return Err(PerformanceMutationError::Cancelled);
            }
            if continuation.inspection.state != pending.before {
                return Err(PerformanceMutationError::Failed);
            }
            let action = match &pending.expected {
                ExpectedComposition::Graph {
                    game_version,
                    loader,
                    ..
                } => {
                    self.prepare_apply(
                        &bound.instance,
                        &continuation.inspection,
                        self.resolution_request(
                            game_version.clone(),
                            loader.clone(),
                            PerformanceMode::Managed,
                        ),
                    )
                    .await?
                }
                ExpectedComposition::Absent => Action::Remove,
                ExpectedComposition::Snapshot {
                    snapshot_id,
                    prepared: Some(_),
                } => Action::Rollback(
                    select_snapshot(
                        &continuation.inspection.rollback_snapshots,
                        Some(snapshot_id),
                    )?
                    .clone(),
                ),
                _ => return Err(PerformanceMutationError::Unsettled),
            };
            if action.expected() != pending.expected {
                return Err(PerformanceMutationError::PlanUnavailable);
            }
            let inspection = self.recover_bound(bound).await?;
            if inspection.health == axial_performance::BundleHealth::Invalid {
                return Err(PerformanceMutationError::Unsettled);
            }
            if inspection.state != pending.before {
                return Err(PerformanceMutationError::Failed);
            }
            Ok((inspection, action))
        }
        .await;
        match result {
            Ok((inspection, action)) => {
                self.execute(
                    bound.clone(),
                    inspection,
                    action,
                    cancel,
                    &pending.operation_id,
                    Some(pending),
                )
                .await
            }
            Err(error) => {
                if matches!(
                    error,
                    PerformanceMutationError::Cancelled
                        | PerformanceMutationError::Failed
                        | PerformanceMutationError::PlanUnavailable
                        | PerformanceMutationError::SnapshotNotFound
                        | PerformanceMutationError::SnapshotUnavailable
                ) {
                    self.finish(pending, Some(&error))?;
                }
                Err(error)
            }
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

    async fn bind(
        &self,
        instance: RegisteredInstance,
    ) -> Result<BoundInstance, PerformanceMutationError> {
        instance
            .validate_current()
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        if let Some(bound) = self
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(instance.record().instance.id.as_str())
            .and_then(|r| r.bound.clone())
        {
            return Ok(bound);
        }
        let (authority, identity) = self
            .rules
            .manager()
            .bind_admitted_instance(
                instance.record().instance.id.as_str(),
                instance.directory().capability().clone(),
            )
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        let effects = authority
            .bind_instance_effect_authority(&identity)
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?;
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
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        let checkpoint = if !has_pending(&self.storage, &bound.instance.record().instance.id)? {
            let checkpoint = PendingOperation {
                operation_id: uuid::Uuid::new_v4().to_string(),
                instance_id: bound.instance.record().instance.id.clone(),
                directory_receipt: bound
                    .instance
                    .directory()
                    .receipt()
                    .map_err(|_| PerformanceMutationError::Unsettled)?,
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
                Err(PerformanceMutationError::Unsettled)
            }
        }
    }

    async fn execute(
        &self,
        bound: BoundInstance,
        before: ManagedCompositionInspection,
        action: Action,
        cancel: CancellationToken,
        operation_id: &str,
        existing: Option<&PendingOperation>,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        if cancel.is_cancelled() {
            if let Some(existing) = existing {
                self.finish(existing, Some(&PerformanceMutationError::Cancelled))?;
            }
            return Err(PerformanceMutationError::Cancelled);
        }
        let mut pending = PendingOperation {
            operation_id: operation_id.into(),
            instance_id: bound.instance.record().instance.id.clone(),
            directory_receipt: bound
                .instance
                .directory()
                .receipt()
                .map_err(|_| PerformanceMutationError::InstanceUnavailable)?,
            before: before.state,
            expected: action.expected(),
            target_effect_started: false,
            result: None,
        };
        if let Some(existing) = existing {
            if existing != &pending
                || self
                    .storage
                    .read(|db| read_pending(db, &pending.instance_id))?
                    .as_ref()
                    != Some(existing)
            {
                return Err(PerformanceMutationError::Unsettled);
            }
            self.storage
                .read(|db| pending_command(db, &pending))?
                .filter(|command| prepared_command(&pending, command))
                .ok_or(PerformanceMutationError::Unsettled)?;
        } else {
            self.begin(&pending)?;
        }
        self.retain_claimed(bound.instance.clone(), Some(bound.clone()), true);
        #[cfg(any(test, feature = "test-support"))]
        crash_checkpoint_for_test("prepared");
        let result = match action {
            Action::Apply(plan) => {
                plan.ensure_current()?;
                let service = self.clone();
                let mut checkpoint = pending.clone();
                let instance = bound.instance.clone();
                bound
                    .authority
                    .ensure_installed(
                        &bound.identity,
                        &bound.effects,
                        plan.plan(),
                        self.transfers.clone(),
                        move || async move {
                            if cancel.is_cancelled() {
                                return Err(PerformanceMutationError::Cancelled);
                            }
                            instance
                                .validate_current()
                                .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
                            checkpoint.target_effect_started = true;
                            service.save(&checkpoint)
                        },
                    )
                    .await
                    .map(|outcome| {
                        pending.target_effect_started = outcome.target_changed();
                        Some(outcome.into_state())
                    })
                    .map_err(|error| match error {
                        axial_performance::ManagedInstallExecutionError::BeforeTargetEffect {
                            error,
                            ..
                        } => error,
                        axial_performance::ManagedInstallExecutionError::Mutation { .. } => {
                            PerformanceMutationError::Failed
                        }
                    })
            }
            Action::Remove => {
                pending.target_effect_started = true;
                self.save(&pending)?;
                #[cfg(any(test, feature = "test-support"))]
                crash_checkpoint_for_test("effect");
                bound
                    .authority
                    .remove_managed(&bound.identity, &bound.effects)
                    .await
                    .map(|_| None)
                    .map_err(|_| PerformanceMutationError::Failed)
            }
            Action::Rollback(snapshot) => {
                pending.target_effect_started = true;
                self.save(&pending)?;
                #[cfg(any(test, feature = "test-support"))]
                crash_checkpoint_for_test("effect");
                bound
                    .authority
                    .rollback_managed_snapshot(&bound.identity, &bound.effects, &snapshot.id)
                    .await
                    .map(|outcome| match outcome {
                        ManagedRollbackOutcome::ManagedComposition(state) => Some(state),
                        ManagedRollbackOutcome::ManagedStateAbsent => None,
                    })
                    .map_err(|_| PerformanceMutationError::Failed)
            }
        };
        if let Ok(state) = &result {
            pending.result = Some(CompletedComposition::from_state(state.clone()));
            self.save(&pending)?;
        }
        pending = self
            .storage
            .read(|db| read_pending(db, &pending.instance_id))?
            .filter(|stored| same_preparation(stored, &pending))
            .ok_or(PerformanceMutationError::Unsettled)?;
        let inspection = self.recover_bound(&bound).await?;
        if inspection.health == axial_performance::BundleHealth::Invalid {
            return Err(PerformanceMutationError::Unsettled);
        }
        let completed = pending.result.as_ref().map(CompletedComposition::state);
        match classify_recovered(
            pending.before.as_ref(),
            &pending.expected,
            completed.as_ref(),
            inspection.state.as_ref(),
        ) {
            RestartDisposition::Applied => {
                self.finish(&pending, None)?;
                Ok(inspection)
            }
            RestartDisposition::RestoredBefore => {
                let error = result.err().unwrap_or(PerformanceMutationError::Failed);
                self.finish(&pending, Some(&error))?;
                Err(error)
            }
            RestartDisposition::Preserve => Err(PerformanceMutationError::Unsettled),
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
    fn pending(&self) -> Result<Vec<PendingOperation>, PerformanceMutationError> {
        self.storage.read(|connection| {
            let mut query = connection.prepare("SELECT instance_id,operation_id,payload FROM performance_operations ORDER BY instance_id LIMIT ?1")?;
            let mut rows = query.query([MAX_PENDING_OPERATIONS + 1])?;
            let mut pending = Vec::new();
            let mut total_bytes = 0usize;
            while let Some(row) = rows.next()? {
                let bytes = row.get_ref(2)?.as_blob().map_err(|_| PerformanceMutationError::Unsettled)?;
                if bytes.len() > MAX_PENDING_ROW_BYTES {
                    return Err(PerformanceMutationError::Unsettled);
                }
                total_bytes = total_bytes.checked_add(bytes.len()).ok_or(PerformanceMutationError::Unsettled)?;
                check_pending_budget(pending.len() + 1, total_bytes)?;
                let instance = row.get_ref(0)?.as_str().map_err(|_| PerformanceMutationError::Unsettled)?;
                let operation = row.get_ref(1)?.as_str().map_err(|_| PerformanceMutationError::Unsettled)?;
                pending.push(decode_pending(instance, operation, bytes)?);
            }
            Ok(pending)
        })
    }
    fn begin(&self, pending: &PendingOperation) -> Result<(), PerformanceMutationError> {
        let bytes = serde_json::to_vec(pending).map_err(|_| PerformanceMutationError::Unsettled)?;
        self.storage.transaction(|tx| {
            let (count, total_bytes) = pending_inventory(tx)?;
            check_pending_budget(count + 1, total_bytes.checked_add(bytes.len()).ok_or(PerformanceMutationError::Unsettled)?)?;
            let command = pending_command(tx, pending)?;
            if command.is_none() && !matches!(pending.expected, ExpectedComposition::Inspection) {
                return Err(PerformanceMutationError::Unsettled);
            }
            if tx.execute("INSERT INTO performance_operations(instance_id,operation_id,payload) VALUES(?1,?2,?3)", params![pending.instance_id.as_str(), pending.operation_id, bytes])? != 1
                || read_pending(tx, &pending.instance_id)?.as_ref() != Some(pending)
                || pending_command(tx, pending)? != command {
                return Err(PerformanceMutationError::Unsettled);
            }
            pending_inventory(tx)?;
            Ok(())
        })
    }
    fn save(&self, pending: &PendingOperation) -> Result<(), PerformanceMutationError> {
        let bytes = serde_json::to_vec(pending).map_err(|_| PerformanceMutationError::Unsettled)?;
        self.storage.transaction(|tx| {
            let (count, total_bytes) = pending_inventory(tx)?;
            let previous = read_pending(tx, &pending.instance_id)?.ok_or(PerformanceMutationError::Unsettled)?;
            if !same_preparation(&previous, pending)
                || (previous.target_effect_started && !pending.target_effect_started)
                || (previous.result.is_some() && previous.result != pending.result) {
                return Err(PerformanceMutationError::Unsettled);
            }
            let previous_bytes: usize = tx.query_row("SELECT length(payload) FROM performance_operations WHERE instance_id=?1", [pending.instance_id.as_str()], |row| row.get(0))?;
            check_pending_budget(count, total_bytes.checked_sub(previous_bytes).and_then(|size| size.checked_add(bytes.len())).ok_or(PerformanceMutationError::Unsettled)?)?;
            let command = pending_command(tx, pending)?.ok_or(PerformanceMutationError::Unsettled)?;
            if tx.execute("UPDATE performance_operations SET payload=?1 WHERE instance_id=?2 AND operation_id=?3", params![bytes, pending.instance_id.as_str(), pending.operation_id])? != 1 { return Err(PerformanceMutationError::Unsettled); }
            if read_pending(tx, &pending.instance_id)?.as_ref() != Some(pending)
                || pending_command(tx, pending)?.as_ref() != Some(&command) {
                return Err(PerformanceMutationError::Unsettled);
            }
            pending_inventory(tx)?;
            Ok(())
        })
    }
    fn finish(
        &self,
        pending: &PendingOperation,
        failure: Option<&PerformanceMutationError>,
    ) -> Result<(), PerformanceMutationError> {
        self.storage.transaction(|tx| {
            if read_pending(tx, &pending.instance_id)?.as_ref() != Some(pending) {
                return Err(PerformanceMutationError::Unsettled);
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
                return Err(PerformanceMutationError::Unsettled);
            }
            if read_pending(tx, &pending.instance_id)?.is_some() {
                return Err(PerformanceMutationError::Unsettled);
            }
            if let Some(status) = &command {
                let saved: (String, String, Vec<u8>) = tx.query_row(
                    "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
                    [&status.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if command_status(&status.id, &saved.0, &saved.1, &saved.2)? != *status {
                    return Err(PerformanceMutationError::Unsettled);
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

enum Action {
    Apply(PreparedManagedPlan),
    Remove,
    Rollback(axial_performance::RollbackSnapshotSummary),
}

impl Action {
    fn expected(&self) -> ExpectedComposition {
        match self {
            Self::Apply(plan) => ExpectedComposition::from_plan(plan.plan()),
            Self::Remove => ExpectedComposition::Absent,
            Self::Rollback(snapshot) => ExpectedComposition::from_snapshot(snapshot),
        }
    }
}

fn same_preparation(left: &PendingOperation, right: &PendingOperation) -> bool {
    left.operation_id == right.operation_id
        && left.instance_id == right.instance_id
        && left.directory_receipt == right.directory_receipt
        && left.before == right.before
        && left.expected == right.expected
}

fn validate_target(
    instance: &RegisteredInstance,
    request: &ResolutionRequest,
) -> Result<(), PerformanceMutationError> {
    let record = &instance.record().instance;
    let loader = if record.loader_key.is_empty() {
        "vanilla"
    } else {
        record.loader_key.as_str()
    };
    if request.game_version != record.minecraft_version
        || !request.loader.eq_ignore_ascii_case(loader)
    {
        return Err(PerformanceMutationError::PlanUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn budget_checkpoint(service: &PerformanceService) -> PendingOperation {
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
        service: &PerformanceService,
    ) -> Result<PerformanceService, PerformanceMutationError> {
        PerformanceService::new(
            service.storage.clone(),
            service.instances.clone(),
            service.tasks.clone(),
            service.content.clone(),
            service.transfers.clone(),
        )
    }

    fn padded_checkpoint(
        service: &PerformanceService,
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
            Err(PerformanceMutationError::Unsettled)
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
            Err(PerformanceMutationError::Unsettled)
        ));
        assert!(matches!(
            restore_budget_service(&service),
            Err(PerformanceMutationError::Unsettled)
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
            Err(PerformanceMutationError::Unsettled)
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
            Err(PerformanceMutationError::Unsettled)
        ));
        assert!(matches!(
            restore_budget_service(&service),
            Err(PerformanceMutationError::Unsettled)
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
            Err(PerformanceMutationError::Unsettled)
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

    #[tokio::test]
    async fn pending_budget_failed_result_publication_preserves_prior_proof_and_reservation() {
        for boundary in ["before", "after"] {
            let (_root, service, admitted) = launch_fixture().await;
            let before = super::super::duplicate::seed_managed(&admitted).await;
            let id = admitted.record().instance.id.clone();
            let receipt = admitted.directory().receipt().unwrap();
            let path = admitted.directory().read_projection().unwrap();
            std::fs::write(path.join("mods/user.jar"), b"untouched user file").unwrap();
            drop(admitted);
            let mut fillers = Vec::new();
            for _ in 0..4 {
                let pending = budget_checkpoint(&service).await;
                service.begin(&pending).unwrap();
                fillers.push(pending);
            }
            for filler in &fillers[..3] {
                padded_checkpoint(&service, filler, MAX_PENDING_ROW_BYTES);
            }
            let filler_before =
                padded_checkpoint(&service, &fillers[3], MAX_PENDING_ROW_BYTES - 32768);
            let (predicate, over) = if boundary == "before" {
                ("IS NULL", 0)
            } else {
                ("IS NOT NULL", 1)
            };
            service.storage.transaction(|tx| {
                tx.execute_batch(&format!("CREATE TRIGGER fill_pending AFTER UPDATE ON performance_operations WHEN NEW.instance_id='{id}' AND json_extract(CAST(NEW.payload AS TEXT),'$.target_effect_started')=1 AND json_extract(CAST(NEW.payload AS TEXT),'$.result') {predicate} BEGIN UPDATE performance_operations SET payload=CAST(CAST(payload AS TEXT)||printf('%*s',{}-length(NEW.payload)+{over}-length(payload),'') AS BLOB) WHERE instance_id='{}'; END;", MAX_PENDING_ROW_BYTES, fillers[3].instance_id))?;
                Ok::<_, StorageError>(())
            }).unwrap();
            assert!(matches!(
                service.remove(&id).await,
                Err(PerformanceMutationError::Unsettled)
            ));
            wait_for_mutation_tasks(&service).await;
            let command = service.instance_operation(&id).unwrap().unwrap();
            assert_eq!(command.state, "unsettled");
            let expected = PendingOperation {
                operation_id: command.id,
                instance_id: id.clone(),
                directory_receipt: receipt,
                before: Some(before),
                expected: ExpectedComposition::Absent,
                target_effect_started: true,
                result: None,
            };
            let persisted: Vec<u8> = service
                .storage
                .read(|db| {
                    Ok::<_, StorageError>(db.query_row(
                        "SELECT payload FROM performance_operations WHERE instance_id=?1",
                        [id.as_str()],
                        |row| row.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(persisted, serde_json::to_vec(&expected).unwrap());
            assert!(!path.join("mods/current.jar").exists());
            assert_eq!(
                std::fs::read(path.join("mods/user.jar")).unwrap(),
                b"untouched user file"
            );
            assert!(service.instances.admit(&id).is_err());
            assert!(service.has_unsettled_effects());
            if boundary == "after" {
                let saved: Vec<u8> = service
                    .storage
                    .read(|db| {
                        Ok::<_, StorageError>(db.query_row(
                            "SELECT payload FROM performance_operations WHERE instance_id=?1",
                            [fillers[3].instance_id.as_str()],
                            |row| row.get(0),
                        )?)
                    })
                    .unwrap();
                assert_eq!(saved, filler_before);
            } else {
                assert_eq!(
                    service.storage.read(pending_inventory).unwrap().1,
                    MAX_PENDING_BYTES
                );
            }
            service
                .storage
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER fill_pending")?;
                    Ok::<_, StorageError>(())
                })
                .unwrap();
            for filler in &fillers {
                service.finish(filler, None).unwrap();
            }
            // This Remove has independently verifiable absence; a lost rollback
            // result would not acquire settlement proof merely by freeing bytes.
            assert_eq!(service.recover_pending().await.unwrap(), 1);
            assert_eq!(service.pending_count().unwrap(), 0);
            assert!(!service.has_unsettled_effects());
            service.instances.library().try_preserve().unwrap();
        }
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
                Ok::<_, PerformanceMutationError>(())
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
            Err(PerformanceMutationError::Unsettled)
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
            Err(PerformanceMutationError::Unsettled)
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
            Err(PerformanceMutationError::Unsettled)
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
                Ok::<_, PerformanceMutationError>(())
            })
            .unwrap();
        assert!(matches!(
            restore_budget_service(&service),
            Err(PerformanceMutationError::Unsettled)
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
                    Ok::<_, PerformanceMutationError>(())
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

    #[cfg(target_os = "linux")]
    fn fixture_lease_holders(root: &std::path::Path) -> String {
        use std::os::unix::fs::MetadataExt;

        let lease = match std::fs::metadata(root.join(".axial-root.lease")) {
            Ok(lease) => lease,
            Err(error) => return format!("lease metadata unavailable: {:?}", error.kind()),
        };
        let (major, minor) = (
            libc::major(lease.dev()) as u64,
            libc::minor(lease.dev()) as u64,
        );
        let inode = lease.ino();
        let fds = std::fs::read_dir("/proc/self/fd")
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        std::fs::metadata(entry.path()).is_ok_and(|metadata| {
                            metadata.dev() == lease.dev() && metadata.ino() == inode
                        })
                    })
                    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
                    .take(16)
                    .collect::<Vec<_>>()
            })
            .map_err(|error| error.kind());
        let locks = std::fs::read_to_string("/proc/locks")
            .map(|text| {
                text.lines()
                    .filter(|line| {
                        line.split_whitespace().any(|field| {
                            let mut identity = field.split(':');
                            u64::from_str_radix(identity.next().unwrap_or(""), 16) == Ok(major)
                                && u64::from_str_radix(identity.next().unwrap_or(""), 16)
                                    == Ok(minor)
                                && identity.next().unwrap_or("").parse::<u64>() == Ok(inode)
                                && identity.next().is_none()
                        })
                    })
                    .take(16)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .map_err(|error| error.kind());
        format!("lease={major:x}:{minor:x}:{inode}; self_fds={fds:?}; locks={locks:?}")
    }

    fn restart_service(
        root: &std::path::Path,
        library_id: crate::library::LibraryId,
        context: &str,
        prior: Option<&crate::library::LibrarySnapshot>,
    ) -> PerformanceService {
        use crate::{
            instances::directory::Registry,
            library::{LibraryLifecycle, LibraryOpenOutcome},
            network::{ClientConfig, ProviderClient},
            tasks::Exclusions,
        };
        let library = match LibraryLifecycle::open_with_id(root, library_id) {
            LibraryOpenOutcome::Ready(library) => library,
            LibraryOpenOutcome::NoEffect(axial_fs::RootSessionError::Busy) => {
                #[cfg(target_os = "linux")]
                let lease = fixture_lease_holders(root);
                #[cfg(not(target_os = "linux"))]
                let lease = "lease holder diagnostics unavailable on this platform";
                panic!("restart fixture library busy ({context}); prior={prior:?}; {lease}");
            }
            other => panic!("restart fixture library unavailable ({context}): {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::content::install::MIGRATION,
                MIGRATION,
                super::super::rules::MIGRATION,
            ])
            .unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage.clone()), library, Exclusions::new());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let content = ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap())
            .unwrap()
            .with_cancellation(cancelled);
        let mut service = PerformanceService::new(
            storage.clone(),
            directories,
            TaskOwner::new(8).unwrap(),
            Arc::new(content),
            super::super::public_transfer_resolver(),
        )
        .unwrap();
        service.rules = PerformanceRules::with_remote(storage, None, None).unwrap();
        service
    }

    #[tokio::test]
    #[ignore = "isolated process fixture invoked by prepared_remove_restart tests"]
    async fn prepared_remove_crash_child() {
        use crate::instances::create::{CreateTarget, InstanceService, tests::request};
        let root =
            std::path::PathBuf::from(std::env::var_os("AXIAL_PERFORMANCE_PREPARED_ROOT").unwrap());
        let library_id = crate::library::LibraryId::parse(
            &std::env::var("AXIAL_PERFORMANCE_PREPARED_LIBRARY").unwrap(),
        )
        .unwrap();
        let service = restart_service(&root, library_id, "child preparation", None);
        let instances = InstanceService::new(service.instances.clone(), service.tasks.clone());
        let mut request = request("Prepared remove restart");
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
        let admitted = service.instances.admit(&instance.id).unwrap();
        super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        std::fs::write(path.join("mods/user.jar"), b"unrelated user artifact").unwrap();
        let action = std::env::var("AXIAL_PERFORMANCE_PREPARED_ACTION").unwrap();
        let snapshot = if action == "rollback" {
            let bound = service.bind(admitted.clone()).await.unwrap();
            let inspection = service.recover_bound(&bound).await.unwrap();
            Some(
                inspection
                    .rollback_snapshots
                    .iter()
                    .find(|snapshot| {
                        !snapshot.latest
                            && snapshot.rollback_available
                            && snapshot.target
                                == axial_performance::RollbackSnapshotTarget::ManagedStateAbsent
                    })
                    .unwrap()
                    .id
                    .clone(),
            )
        } else {
            None
        };
        drop(admitted);
        let request = service.resolution_request(
            "1.21.4".into(),
            "fabric".into(),
            if matches!(action.as_str(), "apply" | "reapply") {
                PerformanceMode::Custom
            } else {
                PerformanceMode::Managed
            },
        );
        if std::env::var("AXIAL_PERFORMANCE_PREPARED_ENTRY").unwrap() == "queued" {
            service
                .submit(&instance.id, request, &action, snapshot)
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !service.tasks.status().is_idle() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            let _ = match action.as_str() {
                "apply" => service.apply(&instance.id, request).await,
                "reapply" => service.reapply(&instance.id, request).await,
                "rollback" => service.rollback(&instance.id, snapshot).await,
                _ => service.remove(&instance.id).await,
            };
        }
        panic!("prepared remove did not reach its durable crash boundary");
    }

    async fn prepared_crash_fixture(
        entry: &str,
        action: &str,
        phase: &str,
    ) -> (tempfile::TempDir, crate::library::LibraryId) {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let library_id = crate::library::LibraryId::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "performance::mutation::tests::prepared_remove_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AXIAL_PERFORMANCE_PREPARED_ROOT", root.path())
            .env("AXIAL_PERFORMANCE_PREPARED_LIBRARY", library_id.to_string())
            .env("AXIAL_PERFORMANCE_PREPARED_ENTRY", entry)
            .env("AXIAL_PERFORMANCE_PREPARED_ACTION", action)
            .env("AXIAL_PERFORMANCE_PREPARED_CRASH", phase)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !timed_out,
            "prepared child timed out: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.status.code(),
            Some(if phase == "planning" {
                41
            } else if phase == "effect" {
                43
            } else {
                42
            }),
            "prepared crash boundary not reached: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (root, library_id)
    }

    async fn settled_prepared_fixture(
        entry: &str,
        action: &str,
    ) -> (
        tempfile::TempDir,
        crate::library::LibraryId,
        PerformanceService,
        PerformanceOperationStatus,
        std::path::PathBuf,
    ) {
        let (root, library_id) = prepared_crash_fixture(entry, action, "prepared").await;
        let storage = MetadataStore::open(root.path().join("metadata.sqlite")).unwrap();
        let (pending_bytes, command) = storage
            .read(|connection| {
                let pending: Vec<u8> = connection.query_row(
                    "SELECT payload FROM performance_operations",
                    [],
                    |row| row.get(0),
                )?;
                let command = connection
                    .query_row(
                        "SELECT id,instance_id,state,payload FROM performance_commands",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, Vec<u8>>(3)?,
                            ))
                        },
                    )
                    .optional()?;
                Ok::<_, StorageError>((pending, command))
            })
            .unwrap();
        let pending: PendingOperation = serde_json::from_slice(&pending_bytes).unwrap();
        assert!(pending.before.is_some());
        assert!(if action == "rollback" {
            matches!(
                pending.expected,
                ExpectedComposition::Snapshot {
                    prepared: Some(_),
                    ..
                }
            )
        } else {
            pending.expected == ExpectedComposition::Absent
        });
        assert!(!pending.target_effect_started);
        assert!(pending.result.is_none());
        let (id, instance, state, bytes) =
            command.expect("every accepted explicit entrypoint retains its command identity");
        let command = command_status(&id, &instance, &state, &bytes).unwrap();
        assert_eq!(command.action, action);
        assert_eq!(command.state, "running");
        assert_eq!(command.instance_id, pending.instance_id.as_str());
        assert_eq!(
            command.id, pending.operation_id,
            "Prepared must remain linked to the accepted command"
        );
        drop(storage);

        let service = restart_service(
            root.path(),
            library_id,
            &format!("{entry}/{action}: first reopen after crash"),
            None,
        );
        assert!(service.instances.admit(&pending.instance_id).is_err());
        let bound = service
            .retained
            .lock()
            .unwrap()
            .get(pending.instance_id.as_str())
            .unwrap()
            .instance
            .clone();
        bound
            .directory()
            .verify_receipt(&pending.directory_receipt)
            .unwrap();
        let path = bound.directory().read_projection().unwrap();
        assert!(path.join("mods/current.jar").is_file());
        assert_eq!(
            service
                .storage
                .read(|db| db
                    .query_row("SELECT payload FROM performance_operations", [], |row| row
                        .get::<_, Vec<
                        u8,
                    >>(
                        0
                    ))
                    .map_err(StorageError::from))
                .unwrap(),
            pending_bytes
        );
        drop(bound);
        service.recover_pending().await.unwrap();
        assert_eq!(service.recover_pending().await.unwrap(), 0);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !service.tasks.status().is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let completed = service.operation(&command.id).unwrap().unwrap();
        assert_eq!(completed.state, "complete");
        assert_eq!(completed.id, command.id);
        assert_eq!(completed.action, action);
        assert!(!path.join("mods/current.jar").exists());
        assert_eq!(
            std::fs::read(path.join("mods/user.jar")).unwrap(),
            b"unrelated user artifact"
        );
        assert_eq!(service.pending_count().unwrap(), 0);
        assert!(!service.has_unsettled_effects());
        service.instances.admit(&pending.instance_id).unwrap();
        (root, library_id, service, command, path)
    }

    async fn prepared_remove_restart(entry: &str, action: &str) {
        let (root, library_id, service, command, path) =
            settled_prepared_fixture(entry, action).await;
        let files = payload_files(&path);
        service.instances.library().try_preserve().unwrap();
        let prior = service.instances.library().snapshot();
        drop(service);

        let reopened = restart_service(
            root.path(),
            library_id,
            &format!("{entry}/{action}: second reopen after settlement"),
            Some(&prior),
        );
        assert_eq!(reopened.recover_pending().await.unwrap(), 0);
        assert!(reopened.tasks.status().is_idle());
        assert_eq!(
            reopened.operation(&command.id).unwrap().unwrap().state,
            "complete"
        );
        assert_eq!(payload_files(&path), files);
        reopened.instances.library().try_preserve().unwrap();
    }

    #[tokio::test]
    async fn prepared_remove_restart_continues_queued_command() {
        prepared_remove_restart("queued", "remove").await;
    }

    #[tokio::test]
    async fn prepared_remove_restart_continues_synchronous_command() {
        prepared_remove_restart("synchronous", "remove").await;
    }

    #[tokio::test]
    async fn prepared_restart_preserves_effective_removal_and_requested_action() {
        for entry in ["queued", "synchronous"] {
            for action in ["apply", "reapply"] {
                prepared_remove_restart(entry, action).await;
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepared_restart_releases_library_during_install_spawn() {
        use std::os::unix::process::CommandExt;

        // Isolate the paused fork from unrelated tests' live root leases.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "performance::mutation::tests::prepared_restart_during_install_spawn_child",
                "--ignored",
                "--nocapture",
            ])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let waited = loop {
            match child.try_wait() {
                Ok(Some(_)) => break Ok(()),
                Err(error) => break Err(error),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    break Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        if waited.is_err() {
            // Only this helper and its own descendants belong to this group.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            waited.is_ok() && output.status.success(),
            "isolated restart failed ({waited:?}): {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "isolated process fixture invoked by the concurrent install-spawn restart test"]
    async fn prepared_restart_during_install_spawn_child() {
        use crate::library::{LibraryLifecycle, LibraryOpenOutcome};
        use std::{
            io::{Read, Write},
            os::{fd::AsRawFd, unix::net::UnixStream, unix::process::CommandExt},
            time::{Duration, Instant},
        };

        let (root, library_id, service, command_status, path) =
            settled_prepared_fixture("queued", "apply").await;
        let files = payload_files(&path);
        let pin = service.instances.library().admit().unwrap();
        let (mut gate, child_gate) = UnixStream::pair().unwrap();
        gate.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        gate.set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut command = crate::install::queue::tests::bounded_fd_child_command();
        unsafe {
            command.pre_exec(move || {
                // Only async-signal-safe operations run between fork and exec.
                let fd = child_gate.as_raw_fd();
                let mut byte = 1_u8;
                if libc::write(fd, (&byte as *const u8).cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut ready = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if libc::poll(&mut ready, 1, 30_000) != 1 {
                    return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT));
                }
                if libc::read(fd, (&mut byte as *mut u8).cast(), 1) != 1 || byte != 1 {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            });
        }
        let (observed, released, child) = std::thread::scope(|scope| {
            let spawning = scope.spawn(move || -> std::io::Result<_> {
                let mut child = command.spawn()?;
                let deadline = Instant::now() + Duration::from_secs(45);
                let waited = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Err(error) => break Err(error),
                        Ok(None) if Instant::now() >= deadline => {
                            break Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    }
                };
                if waited.is_err() {
                    let _ = child.kill();
                }
                let reaped = child.wait();
                waited.and(reaped)
            });
            let observed = (|| -> std::io::Result<_> {
                let mut entered = [0_u8];
                gate.read_exact(&mut entered)?;
                if entered != [1] {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                }
                let preservation = service.instances.library().try_preserve();
                let prior = service.instances.library().snapshot();
                drop(service);
                let pin_valid = pin.revalidate();
                let pinned = LibraryLifecycle::open_with_id(root.path(), library_id);
                drop(pin);
                let reopened = LibraryLifecycle::open_with_id(root.path(), library_id);
                Ok((preservation, prior, pin_valid, pinned, reopened))
            })();
            let released = gate.write_all(&[1]);
            (observed, released, spawning.join())
        });
        // All verdicts, including the expected RED, follow release and reap.
        let (preservation, prior, pin_valid, pinned, reopened) = observed.unwrap();
        let pinned_busy = matches!(
            &pinned,
            LibraryOpenOutcome::NoEffect(axial_fs::RootSessionError::Busy)
        );
        let pinned_debug = format!("{pinned:?}");
        let immediate = matches!(&reopened, LibraryOpenOutcome::Ready(_));
        let immediate_debug = format!("{reopened:?}");
        for outcome in [&pinned, &reopened] {
            if let LibraryOpenOutcome::Ready(owner) = outcome {
                owner.try_preserve().unwrap();
            }
        }
        drop((pinned, reopened));
        released.unwrap();
        assert!(child.unwrap().unwrap().success());
        preservation.unwrap();
        pin_valid.unwrap();
        assert!(
            pinned_busy,
            "live pin did not retain the library: {pinned_debug}"
        );
        let after_exec = restart_service(
            root.path(),
            library_id,
            "after install child exit",
            Some(&prior),
        );
        assert_eq!(after_exec.recover_pending().await.unwrap(), 0);
        assert_eq!(
            after_exec
                .operation(&command_status.id)
                .unwrap()
                .unwrap()
                .state,
            "complete"
        );
        assert_eq!(payload_files(&path), files);
        after_exec.instances.library().try_preserve().unwrap();
        assert!(
            immediate,
            "settled library remained leased before child exec: {immediate_debug}"
        );
    }

    #[tokio::test]
    async fn prepared_restart_rollback_uses_original_nonlatest_snapshot() {
        for entry in ["queued", "synchronous"] {
            prepared_remove_restart(entry, "rollback").await;
        }
    }

    async fn wait_for_mutation_tasks(service: &PerformanceService) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !service.tasks.status().is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prepared_restart_never_replays_planning_effect_started_or_unlinked_work() {
        for phase in ["planning", "effect", "unlinked"] {
            let (root, library_id) = prepared_crash_fixture(
                "queued",
                "remove",
                if phase == "unlinked" {
                    "prepared"
                } else {
                    phase
                },
            )
            .await;
            let storage = MetadataStore::open(root.path().join("metadata.sqlite")).unwrap();
            let bytes = storage
                .read(|db| {
                    db.query_row("SELECT payload FROM performance_commands", [], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })
                    .map_err(StorageError::from)
                })
                .unwrap();
            let mut command: PerformanceOperationStatus = serde_json::from_slice(&bytes).unwrap();
            if phase == "unlinked" {
                let original = command.id.clone();
                command.id = uuid::Uuid::new_v4().to_string();
                storage
                    .transaction(|tx| {
                        tx.execute(
                            "UPDATE performance_commands SET id=?1,payload=?2 WHERE id=?3",
                            params![command.id, serde_json::to_vec(&command).unwrap(), original],
                        )?;
                        Ok::<_, StorageError>(())
                    })
                    .unwrap();
            }
            drop(storage);
            let service = restart_service(root.path(), library_id, phase, None);
            let id = command.instance_id.parse().unwrap();
            let path = if phase == "planning" {
                service
                    .instances
                    .admit(&id)
                    .unwrap()
                    .directory()
                    .read_projection()
                    .unwrap()
            } else {
                service
                    .retained
                    .lock()
                    .unwrap()
                    .get(command.instance_id.as_str())
                    .unwrap()
                    .instance
                    .directory()
                    .read_projection()
                    .unwrap()
            };
            let files = payload_files(&path);
            assert_eq!(
                service.recover_pending().await.unwrap(),
                usize::from(phase != "planning")
            );
            assert!(service.tasks.status().is_idle());
            assert_eq!(payload_files(&path), files, "{phase}");
            assert_eq!(
                service.operation(&command.id).unwrap().unwrap().state,
                if phase == "effect" {
                    "failed"
                } else {
                    "interrupted"
                }
            );
            assert_eq!(service.pending_count().unwrap(), 0);
            assert_eq!(service.recover_pending().await.unwrap(), 0);
            service.instances.admit(&id).unwrap();
            service.instances.library().try_preserve().unwrap();
        }
    }

    #[tokio::test]
    async fn prepared_restart_terminal_acknowledgement_is_atomic_and_retryable() {
        for boundary in ["command", "pending"] {
            for refusal in ["IGNORE", "ABORT,'fixture refusal'"] {
                let (_root, service, admitted) = launch_fixture().await;
                super::super::duplicate::seed_managed(&admitted).await;
                let id = admitted.record().instance.id.clone();
                let path = admitted.directory().read_projection().unwrap();
                drop(admitted);
                let trigger = if boundary == "command" {
                    format!(
                        "CREATE TRIGGER refuse_ack BEFORE UPDATE ON performance_commands WHEN NEW.state IN ('complete','unsettled') BEGIN SELECT RAISE({refusal}); END;"
                    )
                } else {
                    format!(
                        "CREATE TRIGGER refuse_ack BEFORE DELETE ON performance_operations WHEN OLD.operation_id IN (SELECT id FROM performance_commands) BEGIN SELECT RAISE({refusal}); END;"
                    )
                };
                service
                    .storage
                    .transaction(|tx| {
                        tx.execute_batch(&trigger)?;
                        Ok::<_, StorageError>(())
                    })
                    .unwrap();
                assert!(service.remove(&id).await.is_err());
                wait_for_mutation_tasks(&service).await;
                let command = service.instance_operation(&id).unwrap().unwrap();
                assert_ne!(command.state, "complete");
                assert!(service.instances.admit(&id).is_err());
                let pending = service.pending().unwrap().pop().unwrap();
                assert_eq!(pending.operation_id, command.id);
                assert!(pending.target_effect_started);
                assert!(matches!(pending.result, Some(CompletedComposition::Absent)));
                assert!(!path.join("mods/current.jar").exists());
                let settled_files = payload_files(&path);
                service
                    .storage
                    .transaction(|tx| {
                        tx.execute_batch("DROP TRIGGER refuse_ack")?;
                        Ok::<_, StorageError>(())
                    })
                    .unwrap();
                assert_eq!(
                    service.recover_pending().await.unwrap(),
                    1,
                    "{boundary} {refusal}"
                );
                assert!(service.tasks.status().is_idle());
                assert_eq!(
                    service.operation(&command.id).unwrap().unwrap().state,
                    "complete"
                );
                assert_eq!(service.pending_count().unwrap(), 0);
                assert_eq!(payload_files(&path), settled_files);
                service.instances.admit(&id).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn prepared_admission_refuses_missing_or_changed_command_acknowledgement() {
        for corruption in ["ignore", "delete", "change", "effect"] {
            let (_root, service, admitted) = launch_fixture().await;
            super::super::duplicate::seed_managed(&admitted).await;
            let id = admitted.record().instance.id.clone();
            let path = admitted.directory().read_projection().unwrap();
            let files = payload_files(&path);
            drop(admitted);
            let trigger = match corruption {
                "ignore" => {
                    "CREATE TRIGGER corrupt_link BEFORE INSERT ON performance_operations WHEN NEW.operation_id IN (SELECT id FROM performance_commands) BEGIN SELECT RAISE(IGNORE); END;"
                }
                "delete" => {
                    "CREATE TRIGGER corrupt_link AFTER INSERT ON performance_operations WHEN NEW.operation_id IN (SELECT id FROM performance_commands) BEGIN DELETE FROM performance_commands WHERE id=NEW.operation_id; END;"
                }
                "change" => {
                    "CREATE TRIGGER corrupt_link AFTER INSERT ON performance_operations WHEN NEW.operation_id IN (SELECT id FROM performance_commands) BEGIN UPDATE performance_commands SET payload=CAST(json_set(CAST(payload AS TEXT),'$.action','reapply') AS BLOB) WHERE id=NEW.operation_id; END;"
                }
                "effect" => {
                    "CREATE TRIGGER corrupt_link AFTER UPDATE ON performance_operations WHEN NEW.operation_id IN (SELECT id FROM performance_commands) BEGIN DELETE FROM performance_commands WHERE id=NEW.operation_id; END;"
                }
                _ => unreachable!(),
            };
            service
                .storage
                .transaction(|tx| {
                    tx.execute_batch(trigger)?;
                    Ok::<_, StorageError>(())
                })
                .unwrap();
            assert!(service.remove(&id).await.is_err(), "{corruption}");
            wait_for_mutation_tasks(&service).await;
            assert_eq!(payload_files(&path), files);
            let command = service.instance_operation(&id).unwrap().unwrap();
            assert_eq!(command.action, "remove");
            assert_ne!(command.state, "complete");
            if corruption == "effect" {
                let pending = service.pending().unwrap().pop().unwrap();
                assert!(!pending.target_effect_started);
                assert!(pending.result.is_none());
                assert!(service.instances.admit(&id).is_err());
            } else {
                assert_eq!(service.pending_count().unwrap(), 0);
            }
            service
                .storage
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER corrupt_link")?;
                    Ok::<_, StorageError>(())
                })
                .unwrap();
            assert_eq!(
                service.recover_pending().await.unwrap(),
                usize::from(corruption == "effect")
            );
            assert_eq!(payload_files(&path), files);
            assert_eq!(
                service.operation(&command.id).unwrap().unwrap().state,
                "failed"
            );
            service.instances.admit(&id).unwrap();
        }
    }

    #[tokio::test]
    async fn prepared_restart_cancellation_settles_but_unknown_leaf_state_stays_reserved() {
        for case in ["cancel", "changed_leaf"] {
            let (root, library_id) = prepared_crash_fixture("queued", "remove", "prepared").await;
            let service = restart_service(root.path(), library_id, case, None);
            let pending = service.pending().unwrap().pop().unwrap();
            let instance = service
                .retained
                .lock()
                .unwrap()
                .get(pending.instance_id.as_str())
                .unwrap()
                .instance
                .clone();
            let bound = service.bind(instance).await.unwrap();
            let inspection = service.recover_bound(&bound).await.unwrap();
            let path = bound.instance.directory().read_projection().unwrap();
            let files = payload_files(&path);
            if case == "cancel" {
                let cancel = CancellationToken::new();
                cancel.cancel();
                assert!(matches!(
                    service
                        .execute(
                            bound.clone(),
                            inspection,
                            Action::Remove,
                            cancel,
                            &pending.operation_id,
                            Some(&pending)
                        )
                        .await,
                    Err(PerformanceMutationError::Cancelled)
                ));
                assert_eq!(service.pending_count().unwrap(), 0);
                assert_eq!(
                    service
                        .operation(&pending.operation_id)
                        .unwrap()
                        .unwrap()
                        .state,
                    "failed"
                );
                assert_eq!(payload_files(&path), files);
            } else {
                let original = std::fs::read(path.join("mods/current.jar")).unwrap();
                std::fs::write(
                    path.join("mods/current.jar"),
                    b"changed after initial recovery",
                )
                .unwrap();
                let continuation = PreparedContinuation {
                    pending: pending.clone(),
                    command: service.operation(&pending.operation_id).unwrap().unwrap(),
                    bound: bound.clone(),
                    inspection,
                };
                let result = service
                    .resume_prepared(&continuation, CancellationToken::new())
                    .await;
                assert!(matches!(&result, Err(PerformanceMutationError::Unsettled)));
                assert_eq!(service.pending().unwrap(), vec![pending.clone()]);
                assert!(service.instances.admit(&pending.instance_id).is_err());
                assert_eq!(
                    std::fs::read(path.join("mods/current.jar")).unwrap(),
                    b"changed after initial recovery"
                );
                service
                    .complete_command(&continuation.command, &result)
                    .unwrap();
                service.unclaim(&pending.instance_id);
                std::fs::write(path.join("mods/current.jar"), &original).unwrap();
                drop(continuation);
                service.recover_pending().await.unwrap();
                wait_for_mutation_tasks(&service).await;
                assert_eq!(
                    service
                        .operation(&pending.operation_id)
                        .unwrap()
                        .unwrap()
                        .state,
                    "failed"
                );
                assert_eq!(
                    std::fs::read(path.join("mods/current.jar")).unwrap(),
                    original
                );
            }
            drop(bound);
            service.instances.admit(&pending.instance_id).unwrap();
            service.instances.library().try_preserve().unwrap();
        }
    }

    #[tokio::test]
    async fn prepared_restart_refuses_changed_rollback_target_or_count() {
        for field in ["target", "composition_id", "artifact_count"] {
            let (root, library_id) = prepared_crash_fixture("queued", "rollback", "prepared").await;
            let storage = MetadataStore::open(root.path().join("metadata.sqlite")).unwrap();
            storage
                .transaction(|tx| {
                    let bytes: Vec<u8> =
                        tx.query_row("SELECT payload FROM performance_operations", [], |row| {
                            row.get(0)
                        })?;
                    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    value["expected"]["prepared"][field] = match field {
                        "target" => serde_json::json!("managed_composition"),
                        "composition_id" => serde_json::json!("different-composition"),
                        _ => serde_json::json!(1),
                    };
                    tx.execute(
                        "UPDATE performance_operations SET payload=?1",
                        [serde_json::to_vec(&value).unwrap()],
                    )?;
                    Ok::<_, StorageError>(())
                })
                .unwrap();
            drop(storage);
            let service = restart_service(root.path(), library_id, field, None);
            let pending = service.pending().unwrap().pop().unwrap();
            let path = service
                .retained
                .lock()
                .unwrap()
                .get(pending.instance_id.as_str())
                .unwrap()
                .instance
                .directory()
                .read_projection()
                .unwrap();
            let files = payload_files(&path);
            service.recover_pending().await.unwrap();
            wait_for_mutation_tasks(&service).await;
            assert_eq!(
                service
                    .operation(&pending.operation_id)
                    .unwrap()
                    .unwrap()
                    .state,
                "failed",
                "{field}"
            );
            assert_eq!(payload_files(&path), files);
            assert_eq!(service.pending_count().unwrap(), 0);
            service.instances.library().try_preserve().unwrap();
        }
    }

    #[tokio::test]
    async fn terminal_commands_are_pruned_and_reopen_without_replay() {
        let (_root, service, admitted) = launch_fixture().await;
        let id = &admitted.record().instance.id;
        let mut commands = Vec::new();
        for _ in 0..130 {
            let mut command = budget_command(id);
            command.state = "complete".into();
            service.save_operation(&command).unwrap();
            commands.push(command.id);
        }
        let count: usize = service
            .storage
            .read(|db| {
                db.query_row("SELECT count(*) FROM performance_commands", [], |row| {
                    row.get(0)
                })
                .map_err(StorageError::from)
            })
            .unwrap();
        assert_eq!(count, 128);
        assert!(service.operation(&commands[0]).unwrap().is_none());
        assert!(service.operation(&commands[1]).unwrap().is_none());
        let latest = service.instance_operation(id).unwrap().unwrap();
        assert_eq!(&latest.id, commands.last().unwrap());
        let reopened = restore_budget_service(&service).unwrap();
        assert_eq!(reopened.operation(&latest.id).unwrap(), Some(latest));
        assert_eq!(reopened.pending_count().unwrap(), 0);
        assert!(!reopened.has_unsettled_effects());
        assert!(reopened.tasks.status().is_idle());
    }

    #[tokio::test]
    async fn latest_performance_operation_requires_live_registry_target() {
        let (_root, service, admitted) = launch_fixture().await;
        let live = service.instance_operation(&admitted.record().instance.id);
        let unknown = service.instance_operation(&InstanceId::new());
        let registry = service.instances.registry();
        let mut pending = admitted.record().instance.clone();
        pending.id = InstanceId::new();
        pending.name = "Reserved Performance target".into();
        let reserved = service
            .storage
            .transaction(|tx| registry.reserve(tx, pending, &admitted.record().library_id))
            .unwrap();
        let reserved_result = service.instance_operation(&reserved.instance.id);
        let deleting = service
            .storage
            .transaction(|tx| {
                registry.mark_deleting(
                    tx,
                    &admitted.record().instance.id,
                    admitted.record().revision,
                )
            })
            .unwrap();
        let deleting_result = service.instance_operation(&deleting.instance.id);
        assert!(matches!(live, Ok(None)));
        assert!(
            matches!(unknown, Err(PerformanceMutationError::InstanceNotFound)),
            "an unknown UUID is not a live instance with empty history"
        );
        assert!(matches!(
            reserved_result,
            Err(PerformanceMutationError::InstanceUnavailable)
        ));
        assert!(matches!(
            deleting_result,
            Err(PerformanceMutationError::InstanceUnavailable)
        ));
        assert!(service.tasks.status().is_idle());
    }

    #[tokio::test]
    async fn latest_performance_operation_deleted_target_keeps_global_history_on_reopen() {
        use crate::instances::{
            create::InstanceService,
            delete::{DeleteIntent, DeletionStatus},
            directory::Registry,
        };
        let (root, service, admitted) = launch_fixture_with_storage(true).await;
        service
            .storage
            .migrate(&[crate::instances::delete::MIGRATION])
            .unwrap();
        let id = admitted.record().instance.id.clone();
        drop(admitted);
        service.remove(&id).await.unwrap();
        let status = service.instance_operation(&id).unwrap().unwrap();
        assert_eq!(status.state, "complete");
        assert_eq!(status.action, "remove");
        assert_eq!(service.operation(&status.id).unwrap(), Some(status.clone()));
        let instances = InstanceService::new(service.instances.clone(), service.tasks.clone());
        let deletion = instances
            .delete(&id, DeleteIntent::KeepFiles, uuid::Uuid::new_v4())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deletion.status, DeletionStatus::Removed);
        let removed_result = service.instance_operation(&id);
        assert_eq!(service.operation(&status.id).unwrap(), Some(status.clone()));
        service
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let library = service.instances.library().clone();
        let content = service.content.clone();
        let transfers = service.transfers.clone();
        drop(instances);
        drop(service);
        let storage = Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
        let directories = InstanceDirectories::new(
            Registry::new(storage.clone()),
            library,
            crate::tasks::Exclusions::new(),
        );
        let reopened = PerformanceService::new(
            storage.clone(),
            directories,
            TaskOwner::new(8).unwrap(),
            content,
            transfers,
        )
        .unwrap();
        assert_eq!(
            reopened.operation(&status.id).unwrap(),
            Some(status.clone())
        );
        let reopened_result = reopened.instance_operation(&id);
        assert_eq!(reopened.pending_count().unwrap(), 0);
        assert!(reopened.tasks.status().is_idle());
        assert!(
            matches!(
                removed_result,
                Err(PerformanceMutationError::InstanceNotFound)
            ),
            "removed instances must not have an instance-scoped latest view"
        );
        assert!(
            matches!(
                reopened_result,
                Err(PerformanceMutationError::InstanceNotFound)
            ),
            "registry absence remains authoritative after reopening"
        );
    }

    #[tokio::test]
    async fn latest_performance_operation_refuses_corrupt_registry_without_hiding_global_history() {
        let (_root, service, admitted) = launch_fixture().await;
        let id = admitted.record().instance.id.clone();
        drop(admitted);
        service.remove(&id).await.unwrap();
        let status = service.instance_operation(&id).unwrap().unwrap();
        assert_eq!(status.state, "complete");
        let global = service.operation(&status.id).unwrap();
        service
            .storage
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute(
                    "UPDATE instances SET record_json='{' WHERE id=?1",
                    [id.as_str()],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            service.instance_operation(&id),
            Err(PerformanceMutationError::Storage(StorageError::Corrupt))
        ));
        assert_eq!(service.operation(&status.id).unwrap(), global);
        assert!(service.tasks.status().is_idle());
    }

    async fn launch_fixture() -> (tempfile::TempDir, PerformanceService, RegisteredInstance) {
        launch_fixture_with_storage(false).await
    }

    async fn launch_fixture_with_storage(
        durable: bool,
    ) -> (tempfile::TempDir, PerformanceService, RegisteredInstance) {
        use crate::{
            instances::{
                create::{CreateTarget, InstanceService, tests::request},
                directory::Registry,
            },
            library::{LibraryLifecycle, LibraryOpenOutcome},
            network::{ClientConfig, ProviderClient},
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
                super::super::rules::MIGRATION,
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
        // A launch-only plan must not ask a provider to resolve or transfer mods.
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let content = ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap())
            .unwrap()
            .with_cancellation(cancelled);
        let mut service = PerformanceService::new(
            storage.clone(),
            directories.clone(),
            tasks,
            Arc::new(content),
            super::super::public_transfer_resolver(),
        )
        .unwrap();
        service.rules = PerformanceRules::with_remote(storage, None, None).unwrap();
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
    async fn launch_managed_mode_preserves_user_sodium_without_resolving_or_applying_a_bundle() {
        let (_root, service, admitted) = launch_fixture().await;
        let path = admitted.directory().read_projection().unwrap();
        std::fs::write(path.join("mods/sodium-fabric.jar"), b"user-selected Sodium").unwrap();
        let before = payload_files(&path);
        let prepared = service
            .prepare_for_launch(
                &admitted,
                service.resolution_request(
                    "1.21.4".into(),
                    "fabric".into(),
                    PerformanceMode::Managed,
                ),
            )
            .await
            .unwrap();
        assert_eq!(prepared.plan().mode, PerformanceMode::Managed);
        assert!(
            prepared
                .plan()
                .mods
                .iter()
                .any(|item| item.project_id == "AANobbMI")
        );
        assert!(
            prepared
                .planned
                .request()
                .installed_mods
                .iter()
                .any(|item| item == "sodium")
        );
        assert_eq!(
            prepared.effective(),
            axial_performance::effective_performance_plan(prepared.plan())
        );
        prepared.validate_current().unwrap();
        assert_eq!(payload_files(&path), before);
        assert!(!path.join("mods/.axial-performance").exists());
        assert_eq!(service.pending_count().unwrap(), 0);
        assert!(!service.has_unsettled_effects());
    }

    #[tokio::test]
    async fn launch_vanilla_and_custom_preserve_managed_artifacts_and_history() {
        let (_root, service, admitted) = launch_fixture().await;
        super::super::duplicate::seed_managed(&admitted).await;
        let path = admitted.directory().read_projection().unwrap();
        let before = payload_files(&path);
        assert!(before.contains_key(std::path::Path::new("mods/current.jar")));
        assert!(before.keys().any(|path| path.ends_with("snapshot.json")));
        for mode in [PerformanceMode::Vanilla, PerformanceMode::Custom] {
            let prepared = service
                .prepare_for_launch(
                    &admitted,
                    service.resolution_request("1.21.4".into(), "fabric".into(), mode),
                )
                .await
                .unwrap();
            assert_eq!(prepared.plan().mode, mode);
            assert!(prepared.plan().mods.is_empty());
            prepared.validate_current().unwrap();
            assert_eq!(payload_files(&path), before);
            assert_eq!(service.pending_count().unwrap(), 0);
            assert!(!service.has_unsettled_effects());
        }
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
        for mode in [
            PerformanceMode::Managed,
            PerformanceMode::Vanilla,
            PerformanceMode::Custom,
        ] {
            assert!(matches!(
                service
                    .prepare_for_launch(
                        &admitted,
                        service.resolution_request("1.21.4".into(), "fabric".into(), mode,)
                    )
                    .await,
                Err(PerformanceMutationError::Unsettled)
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
            PerformanceMutationError::Storage(StorageError::InvalidMigration(private.into()))
                .diagnostic_code(),
            "storage_unavailable"
        );
        assert_eq!(
            PerformanceMutationError::Failed.diagnostic_code(),
            "failed_preserved"
        );
        assert_eq!(
            PerformanceMutationError::Unsettled.diagnostic_code(),
            "unsettled"
        );
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
