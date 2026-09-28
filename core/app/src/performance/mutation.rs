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
        model::InstanceId,
    },
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{self, Connection, OptionalExtension, Transaction, params},
    },
    tasks::{CancellationToken, TaskOwner},
};
use axial_performance::{
    CompositionPlan, CompositionState, ManagedArtifactTransferResolver,
    ManagedCompositionAuthority, ManagedCompositionInspection, ManagedInstanceEffectAuthority,
    ManagedInstanceIdentity, ManagedRollbackOutcome, PerformanceMode, ResolutionRequest,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub const MIGRATION: Migration = Migration {
    id: "performance_operations.v1",
    sql: "CREATE TABLE performance_operations (instance_id TEXT PRIMARY KEY NOT NULL, operation_id TEXT UNIQUE NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<=2097152)) STRICT;
    CREATE TABLE performance_commands (id TEXT PRIMARY KEY NOT NULL, instance_id TEXT NOT NULL, state TEXT NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<=16384)) STRICT;",
};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceOperationStatus {
    pub id: String,
    pub instance_id: String,
    pub action: String,
    pub state: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoricalOperationEvidence>,
}

/// Recorded predecessor evidence, not a resumable command or current state proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalOperationEvidence {
    pub operation_id: String,
    pub sequence: u64,
    pub intent: HistoricalOperationIntent,
    pub terminal: HistoricalOperationTerminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalOperationAction {
    Install,
    Remove,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoricalRollback {
    NotApplicable,
    Available,
    Unavailable,
    Applied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalOperationIntent {
    /// Original source identity. The enclosing status has the destination UUID.
    pub instance_id: String,
    pub requested_action: HistoricalOperationAction,
    pub action: HistoricalOperationAction,
    pub base_target_id: String,
    pub rollback: HistoricalRollback,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoricalPreparedProof {
    InstallPlan {
        graph_sha512: String,
        artifact_count: u64,
        aggregate_bytes: u64,
    },
    RemoveCurrent {
        graph_sha512: String,
        artifact_count: u64,
    },
    ManagedStateAbsent {},
    RollbackSnapshot {
        snapshot_id: String,
        target: HistoricalRollbackTarget,
        artifact_count: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalRollbackTarget {
    ManagedStateAbsent,
    ManagedComposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalOperationPrepared {
    pub result_target_id: String,
    pub proof: HistoricalPreparedProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoricalOperationTerminal {
    Succeeded {
        prepared: HistoricalOperationPrepared,
        changed_target: bool,
        rollback: HistoricalRollback,
    },
    FailedBeforeEffect {
        error: String,
    },
    FailedAfterEffect {
        prepared: HistoricalOperationPrepared,
        changed_target: bool,
        rollback: HistoricalRollback,
        error: String,
    },
    AbandonedBeforeEffect {},
}

impl HistoricalOperationTerminal {
    pub(crate) fn rollback(&self, admitted: HistoricalRollback) -> HistoricalRollback {
        match self {
            Self::Succeeded { rollback, .. } | Self::FailedAfterEffect { rollback, .. } => {
                *rollback
            }
            _ => admitted,
        }
    }
    fn state(&self) -> &'static str {
        match self {
            Self::Succeeded { .. } => "complete",
            Self::FailedBeforeEffect { .. } | Self::FailedAfterEffect { .. } => "failed",
            Self::AbandonedBeforeEffect {} => "interrupted",
        }
    }
    fn error(&self) -> Option<&str> {
        match self {
            Self::FailedBeforeEffect { error } | Self::FailedAfterEffect { error, .. } => {
                Some(error)
            }
            Self::AbandonedBeforeEffect {} => Some("performance operation abandoned before effect"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoricalOperation {
    pub id: String,
    pub instance_id: InstanceId,
    pub created_at: String,
    pub updated_at: String,
    pub evidence: HistoricalOperationEvidence,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationImportError {
    #[error("invalid historical Performance evidence")]
    Invalid,
    #[error("historical Performance evidence conflicts with stored history")]
    Conflict,
    #[error("historical Performance storage is unavailable")]
    Storage(#[from] StorageError),
}
impl From<rusqlite::Error> for OperationImportError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone)]
pub(crate) struct PreparedOperationImport(Vec<(HistoricalOperation, Vec<u8>)>);

impl PreparedOperationImport {
    pub(crate) fn prepare(
        mut records: Vec<HistoricalOperation>,
    ) -> Result<Self, OperationImportError> {
        if records.len() > 128 {
            return Err(OperationImportError::Invalid);
        }
        records.sort_by_key(|record| record.evidence.sequence);
        let mut ids = BTreeSet::new();
        let mut prepared = Vec::with_capacity(records.len());
        for record in records {
            validate_history(&record)?;
            if !ids.insert(record.id.clone()) {
                return Err(OperationImportError::Invalid);
            }
            let bytes = serde_json::to_vec(&record).map_err(|_| OperationImportError::Invalid)?;
            if bytes.len() > 16 * 1024 {
                return Err(OperationImportError::Invalid);
            }
            prepared.push((record, bytes));
        }
        Ok(Self(prepared))
    }

    pub(crate) fn insert_in(&self, tx: &Transaction<'_>) -> Result<(), OperationImportError> {
        for (record, bytes) in &self.0 {
            match stored_history(tx, &record.id)? {
                Some(saved) if saved == *record => {}
                Some(_) => return Err(OperationImportError::Conflict),
                None => {
                    if tx.execute("INSERT INTO performance_commands(id,instance_id,state,payload) VALUES(?1,?2,'historical',?3)", params![record.id, record.instance_id.as_str(), bytes])? != 1 {
                        return Err(OperationImportError::Conflict);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn verify_in(&self, tx: &Transaction<'_>) -> Result<(), OperationImportError> {
        for (record, _) in &self.0 {
            if stored_history(tx, &record.id)?.as_ref() != Some(record) {
                return Err(OperationImportError::Conflict);
            }
        }
        Ok(())
    }
}

fn stored_history(
    db: &Connection,
    id: &str,
) -> Result<Option<HistoricalOperation>, OperationImportError> {
    let row: Option<(String, String, Vec<u8>)> = db
        .query_row(
            "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(instance, state, bytes)| decode_history(id, &instance, &state, &bytes))
        .transpose()
}

fn decode_history(
    id: &str,
    instance: &str,
    state: &str,
    bytes: &[u8],
) -> Result<HistoricalOperation, OperationImportError> {
    if state != "historical" || bytes.len() > 16 * 1024 {
        return Err(OperationImportError::Conflict);
    }
    let record: HistoricalOperation =
        serde_json::from_slice(bytes).map_err(|_| OperationImportError::Conflict)?;
    validate_history(&record).map_err(|_| OperationImportError::Conflict)?;
    if record.id != id || record.instance_id.as_str() != instance {
        return Err(OperationImportError::Conflict);
    }
    Ok(record)
}

fn validate_history(record: &HistoricalOperation) -> Result<(), OperationImportError> {
    use HistoricalOperationAction as Action;
    use HistoricalPreparedProof as Proof;
    use HistoricalRollback as Rollback;
    let valid_time = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .filter(|time| {
                time.with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                    == value
            })
    };
    let evidence = &record.evidence;
    let intent = &evidence.intent;
    let source_id = evidence
        .operation_id
        .strip_prefix("op-")
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    if !record
        .id
        .strip_prefix("legacy-performance-")
        .is_some_and(|id| lower_hex(id, 64))
        || source_id.is_none_or(|id| {
            id.get_version() != Some(uuid::Version::Random)
                || id.get_variant() != uuid::Variant::RFC4122
                || format!("op-{id}") != evidence.operation_id
        })
        || !lower_hex(&intent.instance_id, 16)
        || evidence.sequence == 0
        || !historical_token(&intent.base_target_id, false)
        || [
            intent.game_version.as_deref(),
            intent.loader.as_deref(),
            intent.mode.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !historical_text(value, 96))
        || intent
            .rollback_id
            .as_deref()
            .is_some_and(|value| !historical_token(value, true))
        || valid_time(&record.created_at)
            .zip(valid_time(&record.updated_at))
            .is_none_or(|(created, updated)| created > updated)
        || !matches!(
            (intent.requested_action, intent.action),
            (Action::Install, Action::Install | Action::Remove)
                | (Action::Remove, Action::Remove)
                | (Action::Rollback, Action::Rollback)
        )
        || !match (intent.action, intent.rollback) {
            (Action::Install, Rollback::Available | Rollback::Unavailable) => true,
            (Action::Remove, Rollback::Available) => {
                intent.base_target_id != "performance_composition_lock"
            }
            (Action::Remove, Rollback::Unavailable) => {
                intent.base_target_id == "performance_composition_lock"
            }
            (Action::Rollback, Rollback::Available) => {
                intent.base_target_id != "performance_rollback_snapshot"
            }
            (Action::Rollback, Rollback::Unavailable) => {
                intent.base_target_id == "performance_rollback_snapshot"
            }
            _ => false,
        }
    {
        return Err(OperationImportError::Invalid);
    }
    let prepared = match &evidence.terminal {
        HistoricalOperationTerminal::Succeeded { prepared, .. }
        | HistoricalOperationTerminal::FailedAfterEffect { prepared, .. } => Some(prepared),
        _ => None,
    };
    if let Some(prepared) = prepared {
        if !historical_token(&prepared.result_target_id, true)
            || prepared.result_target_id != intent.base_target_id
        {
            return Err(OperationImportError::Invalid);
        }
        let valid = match (intent.action, intent.rollback, &prepared.proof) {
            (
                Action::Install,
                Rollback::Available | Rollback::Unavailable,
                Proof::InstallPlan {
                    graph_sha512,
                    artifact_count,
                    ..
                },
            )
            | (
                Action::Remove,
                Rollback::Available,
                Proof::RemoveCurrent {
                    graph_sha512,
                    artifact_count,
                },
            ) => lower_hex(graph_sha512, 128) && *artifact_count <= 1_000_000,
            (Action::Remove, Rollback::Unavailable, Proof::ManagedStateAbsent {}) => true,
            (
                Action::Rollback,
                Rollback::Available,
                Proof::RollbackSnapshot {
                    snapshot_id,
                    target,
                    artifact_count,
                },
            ) => {
                historical_token(snapshot_id, true)
                    && *artifact_count <= 1_000_000
                    && ((*target == HistoricalRollbackTarget::ManagedStateAbsent)
                        == (prepared.result_target_id == "performance_managed_state_absent"))
                    && intent
                        .rollback_id
                        .as_ref()
                        .is_none_or(|requested| requested == snapshot_id)
            }
            _ => false,
        };
        if !valid {
            return Err(OperationImportError::Invalid);
        }
    }
    if evidence
        .terminal
        .error()
        .is_some_and(|error| !historical_text(error, 160))
    {
        return Err(OperationImportError::Invalid);
    }
    let valid = match &evidence.terminal {
        HistoricalOperationTerminal::Succeeded {
            prepared,
            changed_target,
            rollback,
        } => match (&prepared.proof, intent.action, rollback) {
            (Proof::ManagedStateAbsent {}, Action::Remove, Rollback::Unavailable) => {
                !changed_target
            }
            (Proof::InstallPlan { .. }, Action::Install, rollback) => {
                (*changed_target && *rollback == Rollback::Available)
                    || (!changed_target && *rollback == intent.rollback)
            }
            (Proof::RemoveCurrent { .. }, Action::Remove, Rollback::Available)
            | (Proof::RollbackSnapshot { .. }, Action::Rollback, Rollback::Applied) => {
                *changed_target
            }
            _ => false,
        },
        HistoricalOperationTerminal::FailedAfterEffect {
            prepared, rollback, ..
        } => matches!(
            (&prepared.proof, intent.action, rollback),
            (
                Proof::InstallPlan { .. },
                Action::Install,
                Rollback::Available | Rollback::Unavailable
            ) | (
                Proof::RemoveCurrent { .. },
                Action::Remove,
                Rollback::Available | Rollback::Unavailable
            ) | (
                Proof::RollbackSnapshot { .. },
                Action::Rollback,
                Rollback::Available | Rollback::Unavailable | Rollback::Applied
            )
        ),
        _ => true,
    };
    if !valid {
        return Err(OperationImportError::Invalid);
    }
    Ok(())
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn historical_text(value: &str, limit: usize) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.chars().count() <= limit
        && !value.chars().any(char::is_control)
        && value.split_whitespace().collect::<Vec<_>>().join(" ") == value
        && !value.contains(['/', '\\'])
        // The predecessor's public evidence codec, not the stricter log codec.
        && ![
            ".jar", ".exe", ".dll", ".dylib", ".so", " -d", "setting user:",
            "uuid of player", "--", "-x", " -cp ", " -classpath ", "token", "secret",
            "password", "provider_payload", "account_id", "username=", "xuid=",
            "authorization", "credential", "bearer ",
        ]
        .iter()
        .any(|part| lower.contains(part))
        && !lower.starts_with("-d")
        && !(value.contains('@') && value.contains('.'))
        && !value.split_whitespace().any(|token| {
            let token = token.trim_matches(|ch: char| {
                !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
            });
            token.split('.').count() >= 3
                && token.split('.').take(3).all(|part| {
                    part.len() >= 12
                        && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                })
        })
        && !value.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))).any(|part| {
            part.len() >= 48
                && part.bytes().any(|byte| byte.is_ascii_alphabetic())
                && part.bytes().any(|byte| byte.is_ascii_digit())
        })
}

fn historical_token(value: &str, canonical: bool) -> bool {
    let token = value.trim();
    let lower = token.to_ascii_lowercase();
    !token.is_empty()
        && token.len() <= 96
        && (!canonical || token == value)
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+:".contains(&byte))
        && ![
            ".jar",
            ".exe",
            ".dll",
            ".dylib",
            ".so",
            "-xmx",
            "-xms",
            "-xx:",
            "--",
            "token",
            "secret",
            "password",
            "provider_payload",
            "account_id",
            "username=",
            "xuid=",
            "authorization",
            "credential",
            "bearer",
        ]
        .iter()
        .any(|part| lower.contains(part))
        && !lower.starts_with("-d")
        && !token
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|part| {
                part.len() >= 48
                    && part.bytes().any(|byte| byte.is_ascii_alphabetic())
                    && part.bytes().any(|byte| byte.is_ascii_digit())
            })
        && !(token.split('.').count() >= 3
            && token.split('.').take(3).all(|part| {
                part.len() >= 12
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
            }))
}

fn command_status(
    id: &str,
    instance: &str,
    state: &str,
    bytes: &[u8],
) -> Result<PerformanceOperationStatus, PerformanceMutationError> {
    if state == "historical" {
        let record = decode_history(id, instance, state, bytes)
            .map_err(|_| PerformanceMutationError::Unsettled)?;
        let action = match record.evidence.intent.requested_action {
            HistoricalOperationAction::Install => "install",
            HistoricalOperationAction::Remove => "remove",
            HistoricalOperationAction::Rollback => "rollback",
        };
        return Ok(PerformanceOperationStatus {
            id: record.id,
            instance_id: record.instance_id.to_string(),
            action: action.into(),
            state: record.evidence.terminal.state().into(),
            error: record.evidence.terminal.error().map(str::to_owned),
            created_at: record.created_at,
            updated_at: record.updated_at,
            history: Some(record.evidence),
        });
    }
    let status: PerformanceOperationStatus =
        serde_json::from_slice(bytes).map_err(|_| PerformanceMutationError::Unsettled)?;
    if status.history.is_some()
        || status.id.starts_with("legacy-performance-")
        || status.id != id
        || status.instance_id != instance
        || status.state != state
    {
        return Err(PerformanceMutationError::Unsettled);
    }
    Ok(status)
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
                "SELECT id,instance_id,state,payload FROM performance_commands WHERE state IN ('queued','running')",
            )?;
            let records = query
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(query);
            for (id, instance, state, bytes) in records {
                let mut status = command_status(&id, &instance, &state, &bytes)?;
                status.state = "interrupted".into();
                status.error =
                    Some("Operation interrupted; inspect the instance before retrying".into());
                let bytes =
                    serde_json::to_vec(&status).map_err(|_| PerformanceMutationError::Unsettled)?;
                if tx.execute(
                    "UPDATE performance_commands SET state=?1,payload=?2 WHERE id=?3",
                    params![status.state, bytes, status.id],
                )? != 1 { return Err(PerformanceMutationError::Unsettled); }
            }
            Ok::<_, PerformanceMutationError>(())
        })?;
        Ok(service)
    }

    pub fn rules(&self) -> &PerformanceRules {
        &self.rules
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
            let record: Option<(String, String, Vec<u8>)> = connection
                .query_row(
                    "SELECT instance_id,state,payload FROM performance_commands WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
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
            let record: Option<(String, String, Vec<u8>)> = connection.query_row("SELECT id,state,payload FROM performance_commands WHERE instance_id=?1 ORDER BY (state='historical'),rowid DESC LIMIT 1", [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
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
        if !matches!(action, "apply" | "reapply" | "remove" | "rollback") {
            return Err(PerformanceMutationError::PlanUnavailable);
        }
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        if matches!(action, "apply" | "reapply") {
            validate_target(&instance, &request)?;
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
            history: None,
        };
        self.save_operation(&status)?;
        let service = self.clone();
        let mut running = status.clone();
        let accepted = self
            .tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                running.state = "running".into();
                if service.save_operation(&running).is_err() {
                    return;
                }
                let result = if matches!(running.action.as_str(), "apply" | "reapply") {
                    service.apply_admitted(instance, request).await
                } else {
                    async {
                        let bound = service.bind(instance).await?;
                        let inspection = service.recover_bound(&bound).await?;
                        let action = if running.action == "rollback" {
                            Action::Rollback(
                                select_snapshot(
                                    &inspection.rollback_snapshots,
                                    snapshot.as_deref(),
                                )?
                                .id
                                .clone(),
                            )
                        } else {
                            Action::Remove
                        };
                        service.execute(bound, inspection, action, cancel).await
                    }
                    .await
                };
                running.state = match &result {
                    Ok(_) => "complete",
                    Err(PerformanceMutationError::Unsettled) => "unsettled",
                    Err(_) => "failed",
                }
                .into();
                running.error = result.err().map(|error| error.to_string());
                let _ = service.save_operation(&running);
            });
        if accepted.is_err() {
            let mut failed = status;
            failed.state = "failed".into();
            failed.error = Some("Task owner refused operation".into());
            self.save_operation(&failed)?;
            return Err(PerformanceMutationError::InstanceUnavailable);
        }
        Ok(status)
    }

    fn save_operation(
        &self,
        status: &PerformanceOperationStatus,
    ) -> Result<(), PerformanceMutationError> {
        if status.history.is_some()
            || status.id.starts_with("legacy-performance-")
            || status.state == "historical"
        {
            return Err(PerformanceMutationError::Unsettled);
        }
        let mut status = status.clone();
        status.updated_at = chrono::Utc::now().to_rfc3339();
        let bytes = serde_json::to_vec(&status).map_err(|_| PerformanceMutationError::Unsettled)?;
        self.storage.transaction(|tx| {
            if tx.execute("INSERT INTO performance_commands(id,instance_id,state,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET state=excluded.state,payload=excluded.payload", params![status.id, status.instance_id, status.state, bytes])? != 1 { return Err(PerformanceMutationError::Unsettled); }
            tx.execute("DELETE FROM performance_commands WHERE state IN ('complete','failed') AND rowid NOT IN (SELECT rowid FROM performance_commands WHERE state<>'historical' ORDER BY rowid DESC LIMIT 128)", [])?;
            Ok(())
        })
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
        self.inspect_with_request(id, None).await
    }

    pub async fn inspect_with_request(
        &self,
        id: &InstanceId,
        request: Option<ResolutionRequest>,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
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
                    Some(request) => bound
                        .authority
                        .resolve_and_inspect(&bound.identity, &bound.effects, request, admit)
                        .await
                        .map(|resolved| resolved.inspection),
                    None => {
                        bound
                            .authority
                            .inspect(&bound.identity, &bound.effects, None, admit)
                            .await
                    }
                };
                match inspection {
                    Ok(inspection) => {
                        if service
                            .pending()?
                            .iter()
                            .any(|pending| pending.operation_id == checkpoint.operation_id)
                        {
                            service.finish(&checkpoint)?;
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
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        self.apply_admitted(instance, request).await
    }

    async fn apply_admitted(
        &self,
        instance: RegisteredInstance,
        request: ResolutionRequest,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        validate_target(&instance, &request)?;
        let service = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                let bound = service.bind(instance).await?;
                let inspection = service.recover_bound(&bound).await?;
                let mut request = request;
                request.installed_mods = inspection.installed_mod_evidence.clone();
                let planned = Arc::new(service.rules.plan(request).await?);
                let action = if planned.plan().mode == PerformanceMode::Managed {
                    Action::Apply(
                        prepare_managed_install(&service.content, planned)
                            .await
                            .map_err(|_| PerformanceMutationError::PlanUnavailable)?,
                    )
                } else {
                    Action::Remove
                };
                service.execute(bound, inspection, action, cancel).await
            })
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?
            .join()
            .await
            .map_err(|_| PerformanceMutationError::Unsettled)?
    }

    pub async fn remove(
        &self,
        id: &InstanceId,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.simple_action(id, None, false).await
    }

    pub async fn rollback(
        &self,
        id: &InstanceId,
        snapshot: Option<String>,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        self.simple_action(id, snapshot, true).await
    }

    async fn simple_action(
        &self,
        id: &InstanceId,
        snapshot: Option<String>,
        rollback: bool,
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        let instance = self
            .instances
            .admit(id)
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        let service = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                let bound = service.bind(instance).await?;
                let inspection = service.recover_bound(&bound).await?;
                let action = if rollback {
                    Action::Rollback(
                        select_snapshot(&inspection.rollback_snapshots, snapshot.as_deref())?
                            .id
                            .clone(),
                    )
                } else {
                    Action::Remove
                };
                service.execute(bound, inspection, action, cancel).await
            })
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?
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
        for pending in self.pending()? {
            let instance = self
                .retained
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(pending.instance_id.as_str())
                .map(|retained| retained.instance.clone())
                .ok_or(PerformanceMutationError::Unsettled)?;
            instance
                .directory()
                .verify_receipt(&pending.directory_receipt)
                .map_err(|_| PerformanceMutationError::Unsettled)?;
            let bound = self.bind(instance).await?;
            let inspection = self.recover_bound(&bound).await?;
            if inspection.health == axial_performance::BundleHealth::Invalid {
                return Err(PerformanceMutationError::Unsettled);
            }
            if matches!(pending.expected, ExpectedComposition::Inspection) {
                self.finish(&pending)?;
                settled += 1;
                continue;
            }
            let result = pending.result.as_ref().map(CompletedComposition::state);
            match classify_recovered(
                pending.before.as_ref(),
                &pending.expected,
                result.as_ref(),
                inspection.state.as_ref(),
            ) {
                RestartDisposition::Applied | RestartDisposition::RestoredBefore => {
                    self.finish(&pending)?;
                    settled += 1;
                }
                RestartDisposition::Preserve => {
                    self.retain(bound.instance.clone(), Some(bound));
                    return Err(PerformanceMutationError::Unsettled);
                }
            }
        }
        Ok(settled)
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
                    self.finish(&checkpoint)?;
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
    ) -> Result<ManagedCompositionInspection, PerformanceMutationError> {
        if cancel.is_cancelled() {
            return Err(PerformanceMutationError::Cancelled);
        }
        let expected = match &action {
            Action::Apply(plan) => ExpectedComposition::from_plan(plan.plan()),
            Action::Remove => ExpectedComposition::Absent,
            Action::Rollback(snapshot_id) => ExpectedComposition::Snapshot {
                snapshot_id: snapshot_id.clone(),
            },
        };
        let mut pending = PendingOperation {
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: bound.instance.record().instance.id.clone(),
            directory_receipt: bound
                .instance
                .directory()
                .receipt()
                .map_err(|_| PerformanceMutationError::InstanceUnavailable)?,
            before: before.state,
            expected,
            target_effect_started: false,
            result: None,
        };
        self.begin(&pending)?;
        self.retain(bound.instance.clone(), Some(bound.clone()));
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
                bound
                    .authority
                    .rollback_managed_snapshot(&bound.identity, &bound.effects, &snapshot)
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
                self.finish(&pending)?;
                Ok(inspection)
            }
            RestartDisposition::RestoredBefore => {
                self.finish(&pending)?;
                Err(result.err().unwrap_or(PerformanceMutationError::Failed))
            }
            RestartDisposition::Preserve => Err(PerformanceMutationError::Unsettled),
        }
    }

    fn retain(&self, instance: RegisteredInstance, bound: Option<BoundInstance>) {
        self.retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                instance.record().instance.id.to_string(),
                RetainedOperation { instance, bound },
            );
    }
    fn pending(&self) -> Result<Vec<PendingOperation>, PerformanceMutationError> {
        self.storage.read(|connection| {
            let mut query = connection.prepare("SELECT instance_id,operation_id,payload FROM performance_operations ORDER BY instance_id")?;
            query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?)))?.map(|row| {
                let (instance_id, operation_id, bytes) = row?;
                let record: PendingOperation = serde_json::from_slice(&bytes).map_err(|_| PerformanceMutationError::Unsettled)?;
                if record.instance_id.as_str() != instance_id || record.operation_id != operation_id || uuid::Uuid::parse_str(&record.operation_id).is_err() {
                    return Err(PerformanceMutationError::Unsettled);
                }
                Ok(record)
            }).collect()
        })
    }
    fn begin(&self, pending: &PendingOperation) -> Result<(), PerformanceMutationError> {
        let bytes = serde_json::to_vec(pending).map_err(|_| PerformanceMutationError::Unsettled)?;
        self.storage.transaction(|tx| {
            tx.execute("INSERT INTO performance_operations(instance_id,operation_id,payload) VALUES(?1,?2,?3)", params![pending.instance_id.as_str(), pending.operation_id, bytes])?;
            Ok(())
        })
    }
    fn save(&self, pending: &PendingOperation) -> Result<(), PerformanceMutationError> {
        let bytes = serde_json::to_vec(pending).map_err(|_| PerformanceMutationError::Unsettled)?;
        self.storage.transaction(|tx| {
            if tx.execute("UPDATE performance_operations SET payload=?1 WHERE instance_id=?2 AND operation_id=?3", params![bytes, pending.instance_id.as_str(), pending.operation_id])? != 1 { return Err(PerformanceMutationError::Unsettled); }
            Ok(())
        })
    }
    fn finish(&self, pending: &PendingOperation) -> Result<(), PerformanceMutationError> {
        self.storage.transaction(|tx| {
            if tx.execute(
                "DELETE FROM performance_operations WHERE instance_id=?1 AND operation_id=?2",
                params![pending.instance_id.as_str(), pending.operation_id],
            )? != 1
            {
                return Err(PerformanceMutationError::Unsettled);
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
    Rollback(String),
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

    fn historical(instance: &InstanceId, sequence: u64) -> HistoricalOperation {
        HistoricalOperation {
            id: format!("legacy-performance-{sequence:064x}"),
            instance_id: instance.clone(),
            created_at: "2024-02-29T12:34:56.000Z".into(),
            updated_at: "2024-02-29T12:35:56.000Z".into(),
            evidence: HistoricalOperationEvidence {
                operation_id: format!("op-00000000-0000-4000-8000-{sequence:012x}"),
                sequence,
                intent: HistoricalOperationIntent {
                    instance_id: "0000000000000001".into(),
                    requested_action: HistoricalOperationAction::Install,
                    action: HistoricalOperationAction::Remove,
                    base_target_id: "performance_composition_lock".into(),
                    rollback: HistoricalRollback::Unavailable,
                    game_version: Some("1.20.1".into()),
                    loader: Some("fabric".into()),
                    mode: Some("custom".into()),
                    rollback_id: None,
                },
                terminal: HistoricalOperationTerminal::Succeeded {
                    prepared: HistoricalOperationPrepared {
                        result_target_id: "performance_composition_lock".into(),
                        proof: HistoricalPreparedProof::ManagedStateAbsent {},
                    },
                    changed_target: false,
                    rollback: HistoricalRollback::Unavailable,
                },
            },
        }
    }

    #[tokio::test]
    async fn historical_commands_survive_live_pruning_restart_and_never_resume() {
        let (_root, service, instance) = launch_fixture().await;
        let id = &instance.record().instance.id;
        let first = historical(id, 1);
        let second = historical(id, 2);
        let batch = PreparedOperationImport::prepare(vec![second.clone(), first.clone()]).unwrap();
        service
            .storage
            .transaction(|tx| batch.insert_in(tx))
            .unwrap();
        assert_eq!(
            service
                .instance_operation(id)
                .unwrap()
                .unwrap()
                .history
                .unwrap()
                .sequence,
            2
        );
        let historical_status = service.operation(&first.id).unwrap().unwrap();
        assert_eq!(historical_status.state, "complete");
        assert_eq!(historical_status.action, "install");
        assert!(historical_status.history.is_some());
        assert!(service.save_operation(&historical_status).is_err());
        let mut latest = String::new();
        for _ in 0..130 {
            latest = uuid::Uuid::new_v4().to_string();
            service
                .save_operation(&PerformanceOperationStatus {
                    id: latest.clone(),
                    instance_id: id.to_string(),
                    action: "remove".into(),
                    state: "complete".into(),
                    error: None,
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    history: None,
                })
                .unwrap();
        }
        service
            .storage
            .transaction(|tx| batch.verify_in(tx))
            .unwrap();
        assert_eq!(service.instance_operation(id).unwrap().unwrap().id, latest);
        let live = service.operation(&latest).unwrap().unwrap();
        assert!(serde_json::to_value(live).unwrap().get("history").is_none());
        let restarted = PerformanceService::new(
            service.storage.clone(),
            service.instances.clone(),
            TaskOwner::new(8).unwrap(),
            service.content.clone(),
            service.transfers.clone(),
        )
        .unwrap();
        assert_eq!(restarted.pending_count().unwrap(), 0);
        assert!(!restarted.has_unsettled_effects());
        assert_eq!(
            restarted.operation(&first.id).unwrap().unwrap().history,
            historical_status.history
        );
        service
            .storage
            .transaction(|tx| batch.verify_in(tx))
            .unwrap();
        service
            .storage
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute(
                    "UPDATE performance_commands SET state='queued' WHERE id=?1",
                    [&first.id],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            PerformanceService::new(
                service.storage.clone(),
                service.instances.clone(),
                TaskOwner::new(8).unwrap(),
                service.content.clone(),
                service.transfers.clone()
            )
            .is_err()
        );
        let saved: String = service
            .storage
            .read(|db| {
                db.query_row(
                    "SELECT state FROM performance_commands WHERE id=?1",
                    [&first.id],
                    |row| row.get(0),
                )
                .map_err(StorageError::from)
            })
            .unwrap();
        assert_eq!(
            saved, "queued",
            "corrupt historical indexing must not be rewritten as a live command"
        );
    }

    #[test]
    fn historical_command_batch_checks_ignored_writes_collisions_and_indexed_evidence() {
        let store = MetadataStore::in_memory().unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let id = InstanceId::new();
        let first = historical(&id, 1);
        let second = historical(&id, 2);
        let batch = PreparedOperationImport::prepare(vec![first.clone(), second.clone()]).unwrap();
        store.transaction(|tx| -> Result<_, StorageError> {
            tx.execute_batch(&format!("CREATE TRIGGER ignore_history BEFORE INSERT ON performance_commands WHEN NEW.id='{}' BEGIN SELECT RAISE(IGNORE); END;", second.id))?;
            Ok(())
        }).unwrap();
        assert!(matches!(
            store.transaction(|tx| batch.insert_in(tx)),
            Err(OperationImportError::Conflict)
        ));
        let count: i64 = store
            .read(|db| {
                db.query_row("SELECT count(*) FROM performance_commands", [], |row| {
                    row.get(0)
                })
                .map_err(StorageError::from)
            })
            .unwrap();
        assert_eq!(count, 0);
        store
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute_batch("DROP TRIGGER ignore_history")?;
                Ok(())
            })
            .unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        let mut changed = first.clone();
        changed.evidence.intent.mode = Some("vanilla".into());
        let conflict = PreparedOperationImport::prepare(vec![changed]).unwrap();
        assert!(matches!(
            store.transaction(|tx| conflict.insert_in(tx)),
            Err(OperationImportError::Conflict)
        ));
        store.transaction(|tx| batch.verify_in(tx)).unwrap();
        let encoded = serde_json::to_vec(&first).unwrap();
        assert!(
            command_status(
                &first.id,
                InstanceId::new().as_str(),
                "historical",
                &encoded
            )
            .is_err()
        );
        assert!(command_status(&second.id, id.as_str(), "historical", &encoded).is_err());
        store
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute("DELETE FROM performance_commands WHERE id=?1", [&first.id])?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            store.transaction(|tx| batch.verify_in(tx)),
            Err(OperationImportError::Conflict)
        ));
    }

    #[test]
    fn historical_command_proofs_use_source_bounds_and_strict_terminal_shapes() {
        let id = InstanceId::new();
        let base = historical(&id, 1);
        let mut public_fragment = base.clone();
        public_fragment.evidence.terminal = HistoricalOperationTerminal::FailedBeforeEffect {
            error: "AppData or .minecraft configuration unavailable".into(),
        };
        assert!(PreparedOperationImport::prepare(vec![public_fragment]).is_ok());
        let mut sensitive = base.clone();
        sensitive.evidence.terminal = HistoricalOperationTerminal::FailedBeforeEffect {
            error: "Įabcdefghijkl.abcdefghijkl.abcdefghijklĮ".into(),
        };
        assert!(PreparedOperationImport::prepare(vec![sensitive]).is_err());
        for invalid in [
            "operation",
            "timestamp",
            "changed",
            "rollback",
            "request",
            "private",
            "unknown",
        ] {
            let mut value = serde_json::to_value(&base).unwrap();
            match invalid {
                "operation" => {
                    value["evidence"]["operation_id"] =
                        serde_json::json!("op-00000000-0000-0000-0000-000000000001")
                }
                "timestamp" => value["updated_at"] = serde_json::json!("2024-02-29T12:35:56Z"),
                "changed" => {
                    value["evidence"]["terminal"]["changed_target"] = serde_json::json!(true)
                }
                "rollback" => {
                    value["evidence"]["terminal"]["rollback"] = serde_json::json!("Available")
                }
                "request" => {
                    value["evidence"]["intent"]["requested_action"] = serde_json::json!("rollback")
                }
                "private" => {
                    value["evidence"]["intent"]["game_version"] =
                        serde_json::json!("/private/source")
                }
                "unknown" => value["evidence"]["terminal"]["unretained"] = serde_json::json!(true),
                _ => unreachable!(),
            }
            let result = serde_json::from_value::<HistoricalOperation>(value)
                .ok()
                .and_then(|record| PreparedOperationImport::prepare(vec![record]).ok());
            assert!(result.is_none(), "{invalid}");
        }
        let mut many = Vec::new();
        for sequence in 1..=129 {
            many.push(historical(&id, sequence));
        }
        assert!(PreparedOperationImport::prepare(many).is_err());
    }

    async fn launch_fixture() -> (tempfile::TempDir, PerformanceService, RegisteredInstance) {
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
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::create::DUPLICATE_WITNESS_MIGRATION,
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
