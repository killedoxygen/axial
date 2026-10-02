//! Immutable predecessor install evidence. Nothing in this owner can enqueue,
//! activate, acknowledge or reconstruct an installation.

use crate::{
    instances::model::InstanceId,
    storage::{
        Migration, StorageError,
        rusqlite::{self, Connection, OptionalExtension, Transaction, params},
    },
};
use axial_minecraft::{
    LoaderComponentId, ManagedInstallActivationContractId, ManagedInstallPublicationEvidenceId,
    installed_version_id_for, parse_build_id,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use ts_rs::TS;

const MAX_RECORD_BYTES: usize = 256 * 1024;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_RECORDS: usize = 128;
pub(crate) const MAX_COMPLETION_PROOF_BYTES: usize = 16 * 1024;
const PAGE_SIZE: usize = 32;
const ID_PREFIX: &str = "legacy-install-";

pub const MIGRATION: Migration = Migration {
    id: "install_history.v1",
    sql: "CREATE TABLE install_history (
        id TEXT PRIMARY KEY NOT NULL,
        source_id TEXT NOT NULL CHECK(length(source_id)=64),
        instance_id TEXT,
        payload BLOB NOT NULL CHECK(length(payload)<=262144)
    ) STRICT;
    CREATE INDEX install_history_source ON install_history(source_id,instance_id,id);",
};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("The installation history is invalid.")]
    Invalid,
    #[error("The installation history conflicts with its preserved evidence.")]
    Conflict,
    #[error("Installation history storage is unavailable.")]
    Storage(#[source] StorageError),
}

impl From<StorageError> for HistoryError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

// These codecs decode original numeric JSON directly, before Value can erase
// duplicate fields. The public counter projection is a separate wire boundary.
macro_rules! content_metrics {
    ($($field:ident),+ $(,)?) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct ContentDownloadMetrics {
            $(pub(crate) $field: u64),+
        }

        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
        #[serde(deny_unknown_fields)]
        #[ts(rename = "InstallHistoryCounters")]
        pub struct HistoryCounters {
            $(pub $field: String),+
        }

        impl From<&ContentDownloadMetrics> for HistoryCounters {
            fn from(value: &ContentDownloadMetrics) -> Self {
                Self { $($field: value.$field.to_string()),+ }
            }
        }
    };
}

content_metrics! {
    checksum_mismatch,
    metadata_invalid,
    metadata_missing,
    interrupted,
    network_failure,
    permission_failure,
    promote_failed,
    provider_failure,
    size_mismatch,
    temp_discarded,
    temp_write_failed,
    written_to_temp,
    promoted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "values",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum SourceMetrics {
    ContentDownload(ContentDownloadMetrics),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceTarget {
    pub(crate) system: String,
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) ownership: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceStep {
    pub(crate) step_id: String,
    pub(crate) phase: String,
    pub(crate) result: String,
    pub(crate) changed_target: Option<SourceTarget>,
    pub(crate) generated_facts: Vec<String>,
    pub(crate) rollback: String,
    pub(crate) guardian_fact_ids: Vec<String>,
    pub(crate) metrics: Option<SourceMetrics>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SourceIntent {
    Generic {},
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceGuardianTerminal {
    pub(crate) diagnosis_id: String,
    pub(crate) action: String,
    pub(crate) memory: Option<SourceGuardianMemory>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceGuardianMemory {
    pub(crate) binding: String,
    pub(crate) target: SourceTarget,
    pub(crate) observed_at: String,
    pub(crate) suppression_until: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceOperation {
    pub(crate) journal_id: String,
    pub(crate) operation_id: String,
    pub(crate) sequence: u64,
    pub(crate) parent_operation_id: Option<String>,
    pub(crate) command: String,
    pub(crate) intent: SourceIntent,
    pub(crate) status: String,
    pub(crate) owner: String,
    pub(crate) ownership: String,
    pub(crate) targets: Vec<SourceTarget>,
    pub(crate) planned_steps: Vec<SourceStep>,
    pub(crate) completed_steps: Vec<SourceStep>,
    pub(crate) failure_point: Option<String>,
    pub(crate) rollback: String,
    pub(crate) guardian_diagnosis_ids: Vec<String>,
    pub(crate) outcome: Option<String>,
    pub(crate) reconciliation_attempt: Option<serde_json::Value>,
    pub(crate) reconciliation_terminal: Option<serde_json::Value>,
    pub(crate) persisted_state_repair_attempt: Option<serde_json::Value>,
    pub(crate) persisted_state_repair_terminal: Option<serde_json::Value>,
    pub(crate) guardian_install_terminal: Option<SourceGuardianTerminal>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(rename = "InstallHistoryPage")]
pub struct HistoryPage {
    pub records: Vec<HistoryRecord>,
    pub next_after: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(rename = "InstallHistoryRecord")]
pub struct HistoryRecord {
    pub id: String,
    pub historical: bool,
    pub source_id: String,
    pub instance_id: Option<String>,
    pub journal_id: String,
    pub operation_id: String,
    // Original ordering, encoded losslessly even above JavaScript's safe range.
    pub sequence: String,
    pub command: String,
    pub targets: Vec<HistoryTarget>,
    pub planned_steps: Vec<HistoryStep>,
    pub completed_steps: Vec<HistoryStep>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_point: Option<String>,
    pub rollback: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(rename = "InstallHistoryTarget")]
pub struct HistoryTarget {
    pub system: String,
    pub kind: String,
    pub id: String,
    pub ownership: String,
}

impl From<&SourceTarget> for HistoryTarget {
    fn from(value: &SourceTarget) -> Self {
        Self {
            system: value.system.clone(),
            kind: value.kind.clone(),
            id: value.id.clone(),
            ownership: value.ownership.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(rename = "InstallHistoryStep")]
pub struct HistoryStep {
    pub step_id: String,
    pub phase: String,
    pub result: String,
    pub changed_target: Option<HistoryTarget>,
    pub generated_facts: Vec<String>,
    pub rollback: String,
    pub metrics: Option<HistoryMetrics>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    content = "values",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[ts(rename = "InstallHistoryMetrics")]
pub enum HistoryMetrics {
    ContentDownload(HistoryCounters),
}

impl From<&SourceStep> for HistoryStep {
    fn from(value: &SourceStep) -> Self {
        Self {
            step_id: value.step_id.clone(),
            phase: value.phase.clone(),
            result: value.result.clone(),
            changed_target: value.changed_target.as_ref().map(HistoryTarget::from),
            generated_facts: value.generated_facts.clone(),
            rollback: value.rollback.clone(),
            metrics: value.metrics.as_ref().map(|metrics| match metrics {
                SourceMetrics::ContentDownload(values) => {
                    HistoryMetrics::ContentDownload(HistoryCounters::from(values))
                }
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedOperation(Arc<ValidatedOperation>);

#[derive(Debug)]
struct ValidatedOperation {
    id: String,
    source_id: String,
    source: SourceOperation,
    legacy_instance_id: Option<String>,
    stored_bytes: usize,
}

impl PreparedOperation {
    pub(crate) fn prepare(source_id: &str, source: SourceOperation) -> Result<Self, HistoryError> {
        if !lower_hex(source_id, 64)
            || serde_json::to_vec(&source)
                .map_err(|_| HistoryError::Invalid)?
                .len()
                > MAX_RECORD_BYTES
        {
            return Err(HistoryError::Invalid);
        }
        let legacy_instance_id = validate_source(&source)?;
        let id = history_id(source_id, &source.operation_id);
        let stored_bytes = serde_json::to_vec(&StoredRef {
            schema: 1,
            id: &id,
            source_id,
            instance_id: legacy_instance_id
                .as_ref()
                .map(|_| "00000000-0000-0000-0000-000000000001"),
            source: &source,
        })
        .map_err(|_| HistoryError::Invalid)?
        .len();
        if stored_bytes > MAX_RECORD_BYTES {
            return Err(HistoryError::Invalid);
        }
        Ok(Self(Arc::new(ValidatedOperation {
            id,
            source_id: source_id.to_owned(),
            source,
            legacy_instance_id,
            stored_bytes,
        })))
    }

    pub(crate) fn legacy_instance_id(&self) -> Option<&str> {
        self.0.legacy_instance_id.as_deref()
    }
}

#[derive(Clone)]
pub(crate) struct PreparedImport(Vec<Arc<BoundOperation>>);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionProof {
    source_id: String,
    ids: Vec<String>,
    digest: String,
}

struct BoundOperation {
    operation: PreparedOperation,
    instance_id: Option<String>,
    bytes: Vec<u8>,
}

#[derive(Serialize)]
struct StoredRef<'a> {
    schema: u8,
    id: &'a str,
    source_id: &'a str,
    instance_id: Option<&'a str>,
    source: &'a SourceOperation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    schema: u8,
    id: String,
    source_id: String,
    instance_id: Option<String>,
    source: SourceOperation,
}

struct ReadRecord {
    operation: PreparedOperation,
    instance_id: Option<String>,
}

impl PreparedImport {
    pub(crate) fn validate(records: &[PreparedOperation]) -> Result<(), HistoryError> {
        Self::validate_with_archived(records, &Self(Vec::new()))
    }

    pub(crate) fn validate_with_archived(
        records: &[PreparedOperation],
        archived: &Self,
    ) -> Result<(), HistoryError> {
        validate_batch(
            records
                .iter()
                .map(|record| (record, record.0.stored_bytes))
                .chain(
                    archived
                        .0
                        .iter()
                        .map(|record| (&record.operation, record.bytes.len())),
                ),
        )
    }

    pub(crate) fn append(&mut self, other: &Self) -> Result<(), HistoryError> {
        validate_batch(
            self.0
                .iter()
                .chain(&other.0)
                .map(|record| (&record.operation, record.bytes.len())),
        )?;
        self.0.extend(other.0.iter().cloned());
        Ok(())
    }

    pub(crate) fn bind_archived(
        source_id: &str,
        records: Vec<PreparedOperation>,
    ) -> Result<Self, HistoryError> {
        if !lower_hex(source_id, 64) || records.len() > MAX_BATCH_RECORDS {
            return Err(HistoryError::Invalid);
        }
        let mut bound = Vec::with_capacity(records.len());
        for operation in records {
            if operation.0.source_id != source_id {
                return Err(HistoryError::Invalid);
            }
            let instance = archived_instance(&operation).ok_or(HistoryError::Invalid)?;
            bound.push(Arc::new(BoundOperation::new(operation, Some(instance))?));
        }
        let bound = Self(bound);
        Self::validate_with_archived(&[], &bound)?;
        Ok(bound)
    }

    pub(crate) fn bind(
        records: Vec<PreparedOperation>,
        legacy_id: &str,
        instance: &InstanceId,
    ) -> Result<Self, HistoryError> {
        Self::validate(&records)?;
        if !lower_hex(legacy_id, 16) {
            return Err(HistoryError::Invalid);
        }
        let mut bound = Vec::with_capacity(records.len());
        for operation in records {
            let instance_id = match operation.legacy_instance_id() {
                None => None,
                Some(id) if id == legacy_id => Some(instance.as_str().to_owned()),
                Some(_) => return Err(HistoryError::Invalid),
            };
            bound.push(Arc::new(BoundOperation::new(operation, instance_id)?));
        }
        Ok(Self(bound))
    }

    pub(crate) fn bind_global(records: Vec<PreparedOperation>) -> Result<Self, HistoryError> {
        Self::validate(&records)?;
        records
            .into_iter()
            .map(|operation| {
                if operation.legacy_instance_id().is_some() {
                    return Err(HistoryError::Invalid);
                }
                BoundOperation::new(operation, None).map(Arc::new)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub(crate) fn completion_proof(
        &self,
        source_id: &str,
    ) -> Result<CompletionProof, HistoryError> {
        self.proof(source_id, CompletionScope::Global)
    }

    pub(crate) fn archived_completion_proof(
        &self,
        source_id: &str,
    ) -> Result<CompletionProof, HistoryError> {
        self.proof(source_id, CompletionScope::Archived)
    }

    fn proof(
        &self,
        source_id: &str,
        scope: CompletionScope,
    ) -> Result<CompletionProof, HistoryError> {
        if !lower_hex(source_id, 64)
            || self.0.iter().any(|record| {
                record.operation.0.source_id != source_id
                    || !scope.matches(&record.operation, record.instance_id.as_deref())
            })
        {
            return Err(HistoryError::Invalid);
        }
        let mut records: Vec<_> = self.0.iter().collect();
        records.sort_unstable_by_key(|record| &record.operation.0.id);
        let ids: Vec<_> = records
            .iter()
            .map(|record| record.operation.0.id.clone())
            .collect();
        let mut digest = completion_hash(source_id, &ids, scope);
        for record in records {
            digest.update((record.bytes.len() as u64).to_be_bytes());
            digest.update(&record.bytes);
        }
        Ok(CompletionProof {
            source_id: source_id.to_owned(),
            ids,
            digest: hex::encode(digest.finalize()),
        })
    }

    pub(crate) fn insert_in(&self, tx: &Transaction<'_>) -> Result<(), HistoryError> {
        let mut remaining = MAX_BATCH_BYTES;
        for record in &self.0 {
            let value = &record.operation.0;
            match stored_row(tx, &value.id, MAX_RECORD_BYTES.min(remaining))? {
                Some((source, instance, bytes)) => {
                    let bytes = bytes.ok_or(HistoryError::Conflict)?;
                    remaining -= bytes.len();
                    if !record.matches(&source, instance.as_deref(), &bytes) {
                        return Err(HistoryError::Conflict);
                    }
                    continue;
                }
                None => {}
            }
            remaining = remaining
                .checked_sub(record.bytes.len())
                .ok_or(HistoryError::Invalid)?;
            if tx.execute(
                "INSERT INTO install_history(id,source_id,instance_id,payload) VALUES(?1,?2,?3,?4)",
                params![value.id, value.source_id, record.instance_id, record.bytes],
            )? != 1
            {
                return Err(HistoryError::Conflict);
            }
        }
        // A later INSERT trigger can affect an earlier row in this same batch.
        self.verify_in(tx)
    }

    pub(crate) fn verify_in(&self, db: &Connection) -> Result<(), HistoryError> {
        let mut remaining = MAX_BATCH_BYTES;
        for record in &self.0 {
            let (source, instance, bytes) =
                stored_row(db, &record.operation.0.id, MAX_RECORD_BYTES.min(remaining))?
                    .ok_or(HistoryError::Conflict)?;
            let bytes = bytes.ok_or(HistoryError::Conflict)?;
            remaining -= bytes.len();
            if !record.matches(&source, instance.as_deref(), &bytes) {
                return Err(HistoryError::Conflict);
            }
        }
        Ok(())
    }
}

fn validate_batch<'a>(
    records: impl IntoIterator<Item = (&'a PreparedOperation, usize)>,
) -> Result<(), HistoryError> {
    let mut ids = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    let mut source_id = None;
    let mut total = 0usize;
    for (record, bytes) in records {
        let value = &record.0;
        total = total.checked_add(bytes).ok_or(HistoryError::Invalid)?;
        if !ids.insert(&value.id)
            || !sequences.insert(value.source.sequence)
            || *source_id.get_or_insert(value.source_id.as_str()) != value.source_id.as_str()
            || ids.len() > MAX_BATCH_RECORDS
            || total > MAX_BATCH_BYTES
        {
            return Err(HistoryError::Invalid);
        }
    }
    Ok(())
}

fn archived_instance(operation: &PreparedOperation) -> Option<String> {
    operation
        .legacy_instance_id()
        .map(|legacy| format!("archived-{}-{legacy}", operation.0.source_id))
}

fn valid_binding(operation: &PreparedOperation, instance: Option<&str>) -> bool {
    match (operation.legacy_instance_id(), instance) {
        (None, None) => true,
        (Some(_), Some(instance)) => {
            instance.parse::<InstanceId>().is_ok()
                || archived_instance(operation).as_deref() == Some(instance)
        }
        _ => false,
    }
}

impl BoundOperation {
    fn new(
        operation: PreparedOperation,
        instance_id: Option<String>,
    ) -> Result<Self, HistoryError> {
        if !valid_binding(&operation, instance_id.as_deref()) {
            return Err(HistoryError::Invalid);
        }
        let value = &operation.0;
        let bytes = serde_json::to_vec(&StoredRef {
            schema: 1,
            id: &value.id,
            source_id: &value.source_id,
            instance_id: instance_id.as_deref(),
            source: &value.source,
        })
        .map_err(|_| HistoryError::Invalid)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(HistoryError::Invalid);
        }
        Ok(Self {
            operation,
            instance_id,
            bytes,
        })
    }

    fn matches(&self, source_id: &str, instance_id: Option<&str>, bytes: &[u8]) -> bool {
        self.operation.0.source_id == source_id
            && self.instance_id.as_deref() == instance_id
            && self.bytes == bytes
    }
}

fn stored_row(
    db: &Connection,
    id: &str,
    max_bytes: usize,
) -> Result<Option<(String, Option<String>, Option<Vec<u8>>)>, HistoryError> {
    let row: Option<(Option<String>, Option<String>, bool, Option<Vec<u8>>)> = db
        .query_row(
            "SELECT
                CASE WHEN length(CAST(source_id AS BLOB))=64 THEN source_id END,
                CASE WHEN length(CAST(instance_id AS BLOB))<=90 THEN instance_id END,
                instance_id IS NULL OR length(CAST(instance_id AS BLOB))<=90,
                CASE WHEN length(CAST(payload AS BLOB))<=?2 THEN payload END
             FROM install_history WHERE id=?1",
            params![id, max_bytes],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(source, instance, binding_bounded, bytes)| {
        if !binding_bounded {
            return Err(HistoryError::Conflict);
        }
        Ok((source.ok_or(HistoryError::Conflict)?, instance, bytes))
    })
    .transpose()
}

fn decode_stored(
    id: &str,
    source_id: &str,
    instance_id: Option<&str>,
    bytes: Option<Vec<u8>>,
) -> Result<ReadRecord, HistoryError> {
    let saved: StoredRecord = serde_json::from_slice(&bytes.ok_or(HistoryError::Conflict)?)
        .map_err(|_| HistoryError::Conflict)?;
    if saved.schema != 1
        || saved.id != id
        || saved.source_id != source_id
        || saved.instance_id.as_deref() != instance_id
    {
        return Err(HistoryError::Conflict);
    }
    let operation = PreparedOperation::prepare(&saved.source_id, saved.source)
        .map_err(|_| HistoryError::Conflict)?;
    if operation.0.id != id || !valid_binding(&operation, saved.instance_id.as_deref()) {
        return Err(HistoryError::Conflict);
    }
    Ok(ReadRecord {
        operation,
        instance_id: saved.instance_id,
    })
}

impl CompletionProof {
    pub(crate) fn count(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn verify_in(&self, db: &Connection, source_id: &str) -> Result<(), HistoryError> {
        read_completed_in(db, source_id, Some(self), None, None).map(|_| ())
    }

    fn validate(&self, source_id: &str) -> Result<(), HistoryError> {
        if self.source_id != source_id
            || !lower_hex(&self.digest, 64)
            || self.ids.len() > MAX_BATCH_RECORDS
            || self.ids.iter().any(|id| !valid_history_id(id))
            || self.ids.windows(2).any(|ids| ids[0] >= ids[1])
        {
            return Err(HistoryError::Conflict);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CompletionScope {
    Global,
    Archived,
}

impl CompletionScope {
    fn matches(self, operation: &PreparedOperation, instance: Option<&str>) -> bool {
        match self {
            Self::Global => operation.legacy_instance_id().is_none() && instance.is_none(),
            Self::Archived => {
                instance.is_some() && archived_instance(operation).as_deref() == instance
            }
        }
    }
}

fn completion_hash(source_id: &str, ids: &[String], scope: CompletionScope) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(match scope {
        CompletionScope::Global => b"axial.legacy.install.completion.v1\0".as_slice(),
        CompletionScope::Archived => b"axial.legacy.content.archived.completion.v1\0".as_slice(),
    });
    hash.update((source_id.len() as u64).to_be_bytes());
    hash.update(source_id.as_bytes());
    hash.update((ids.len() as u64).to_be_bytes());
    for id in ids {
        hash.update((id.len() as u64).to_be_bytes());
        hash.update(id.as_bytes());
    }
    hash
}

/// The metadata owner resolves this exact snapshot from a completed receipt in
/// the same metadata read. Each page verifies at most 128 rows / 8 MiB;
/// unrelated records imported later neither join nor invalidate the snapshot.
pub(crate) fn read_completed_in(
    db: &Connection,
    source_id: &str,
    global: Option<&CompletionProof>,
    archived: Option<&CompletionProof>,
    after: Option<&str>,
) -> Result<HistoryPage, HistoryError> {
    validate_cursor(after)?;
    if !lower_hex(source_id, 64)
        || (global.is_none() && archived.is_none())
        || global
            .map_or(0, CompletionProof::count)
            .saturating_add(archived.map_or(0, CompletionProof::count))
            > MAX_BATCH_RECORDS
    {
        return Err(HistoryError::Conflict);
    }
    let mut remaining = MAX_BATCH_BYTES;
    let mut ids = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    let mut records = BTreeMap::new();
    for (proof, scope) in [
        (global, CompletionScope::Global),
        (archived, CompletionScope::Archived),
    ] {
        let Some(proof) = proof else { continue };
        proof.validate(source_id)?;
        let mut digest = completion_hash(source_id, &proof.ids, scope);
        for id in &proof.ids {
            if !ids.insert(id) {
                return Err(HistoryError::Conflict);
            }
            let (source, instance, bytes) = stored_row(db, id, MAX_RECORD_BYTES.min(remaining))?
                .ok_or(HistoryError::Conflict)?;
            let bytes = bytes.ok_or(HistoryError::Conflict)?;
            remaining -= bytes.len();
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(&bytes);
            let saved = decode_stored(id, &source, instance.as_deref(), Some(bytes))?;
            if source != source_id
                || !scope.matches(&saved.operation, saved.instance_id.as_deref())
                || !sequences.insert(saved.operation.0.source.sequence)
            {
                return Err(HistoryError::Conflict);
            }
            if id.as_str() > after.unwrap_or("") {
                records.insert(id.clone(), saved.project());
                if records.len() > PAGE_SIZE + 1 {
                    records.pop_last();
                }
            }
        }
        if hex::encode(digest.finalize()) != proof.digest {
            return Err(HistoryError::Conflict);
        }
    }
    let has_more = records.len() > PAGE_SIZE;
    if has_more {
        records.pop_last();
    }
    Ok(HistoryPage {
        next_after: has_more.then(|| {
            records
                .last_key_value()
                .expect("full history page")
                .0
                .clone()
        }),
        records: records.into_values().collect(),
    })
}

/// The instance owner supplies its completed import mapping in this same read
/// transaction. Callers cannot select a source by names or version similarity.
pub(crate) fn read_in(
    db: &Connection,
    source_id: &str,
    legacy_id: &str,
    instance: &InstanceId,
    after: Option<&str>,
) -> Result<HistoryPage, HistoryError> {
    validate_cursor(after)?;
    if !lower_hex(source_id, 64) || !lower_hex(legacy_id, 16) {
        return Err(HistoryError::Invalid);
    }
    let mut statement = db.prepare(
        "WITH candidates AS (
           SELECT id FROM (SELECT id FROM install_history
             WHERE source_id=?1 AND instance_id IS NULL AND id>?3 ORDER BY id LIMIT 33)
           UNION ALL
           SELECT id FROM (SELECT id FROM install_history
             WHERE source_id=?1 AND instance_id=?2 AND id>?3 ORDER BY id LIMIT 33)
         )
         SELECT CASE WHEN length(CAST(h.id AS BLOB))=79 THEN h.id END,
           CASE WHEN length(CAST(h.source_id AS BLOB))=64 THEN h.source_id END,
           CASE WHEN length(CAST(h.instance_id AS BLOB))<=36 THEN h.instance_id END,
           CASE WHEN length(CAST(h.payload AS BLOB))<=262144 THEN h.payload END
         FROM (SELECT id FROM candidates ORDER BY id LIMIT 33) page
         JOIN install_history h ON h.id=page.id ORDER BY page.id",
    )?;
    let mut rows = statement.query(params![source_id, instance.as_str(), after.unwrap_or("")])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        if records.len() == PAGE_SIZE {
            return Ok(HistoryPage {
                next_after: records
                    .last()
                    .map(|record: &HistoryRecord| record.id.clone()),
                records,
            });
        }
        let id: String = row.get(0)?;
        let source: String = row.get(1)?;
        let bound: Option<String> = row.get(2)?;
        let saved = decode_stored(&id, &source, bound.as_deref(), row.get(3)?)?;
        if source != source_id
            || saved
                .operation
                .legacy_instance_id()
                .is_some_and(|id| id != legacy_id)
            || saved
                .instance_id
                .as_deref()
                .is_some_and(|id| id != instance.as_str())
        {
            return Err(HistoryError::Conflict);
        }
        records.push(saved.project());
    }
    Ok(HistoryPage {
        records,
        next_after: None,
    })
}

impl ReadRecord {
    fn project(&self) -> HistoryRecord {
        let value = &self.operation.0;
        let source = &value.source;
        HistoryRecord {
            id: value.id.clone(),
            historical: true,
            source_id: value.source_id.clone(),
            instance_id: self.instance_id.clone(),
            journal_id: source.journal_id.clone(),
            operation_id: source.operation_id.clone(),
            sequence: source.sequence.to_string(),
            command: source.command.clone(),
            targets: source.targets.iter().map(HistoryTarget::from).collect(),
            planned_steps: source.planned_steps.iter().map(HistoryStep::from).collect(),
            completed_steps: source
                .completed_steps
                .iter()
                .map(HistoryStep::from)
                .collect(),
            outcome: source.outcome.clone().expect("validated terminal history"),
            failure_point: source.failure_point.clone(),
            rollback: source.rollback.clone(),
        }
    }
}

fn history_id(source_id: &str, operation_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"axial.legacy.install.v1\0");
    hash.update((source_id.len() as u64).to_be_bytes());
    hash.update(source_id.as_bytes());
    hash.update(operation_id.as_bytes());
    format!("{ID_PREFIX}{}", hex::encode(hash.finalize()))
}

fn valid_history_id(value: &str) -> bool {
    value
        .strip_prefix(ID_PREFIX)
        .is_some_and(|value| lower_hex(value, 64))
}

pub(crate) fn validate_cursor(after: Option<&str>) -> Result<(), HistoryError> {
    if after.is_some_and(|id| !valid_history_id(id)) {
        Err(HistoryError::Invalid)
    } else {
        Ok(())
    }
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

enum Identity {
    Vanilla(String),
    Loader { version: String, base: String },
    Content(String),
}

fn validate_source(source: &SourceOperation) -> Result<Option<String>, HistoryError> {
    let id = source
        .operation_id
        .strip_prefix("op-")
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    if id.is_none_or(|id| {
        id.get_version() != Some(uuid::Version::Random)
            || id.get_variant() != uuid::Variant::RFC4122
            || format!("op-{id}") != source.operation_id
    }) || source.journal_id != format!("journal-{}", source.operation_id)
        || source.sequence == 0
        || source.parent_operation_id.is_some()
        || source.owner != "Application"
        || source.ownership != "LauncherManaged"
        || source.rollback != "NotApplicable"
        || source.reconciliation_attempt.is_some()
        || source.reconciliation_terminal.is_some()
        || source.persisted_state_repair_attempt.is_some()
        || source.persisted_state_repair_terminal.is_some()
        || source.targets.len() != 2
        || source.planned_steps.len() != 1
        || source.completed_steps.is_empty()
        || source.completed_steps.len() > 256
    {
        return Err(HistoryError::Invalid);
    }
    let planned = &source.planned_steps[0];
    if planned.phase != "Planning"
        || planned.result != "Planned"
        || planned.changed_target.is_some()
        || planned.rollback != "NotApplicable"
        || !planned.guardian_fact_ids.is_empty()
        || planned.metrics.is_some()
        || source.targets.iter().any(|target| {
            target.system != "Application"
                || target.ownership != "LauncherManaged"
                || !structured_token(&target.id, 96)
        })
    {
        return Err(HistoryError::Invalid);
    }
    let session = source
        .targets
        .iter()
        .find(|target| target.kind == "Session")
        .ok_or(HistoryError::Invalid)?;
    let identity = match source.command.as_str() {
        "InstallVersion" if planned.step_id == "install_version" => {
            let target = source
                .targets
                .iter()
                .find(|target| target.kind == "Version")
                .ok_or(HistoryError::Invalid)?;
            match planned.generated_facts.as_slice() {
                [kind, version] if kind == "install_kind:vanilla" => {
                    let version = version
                        .strip_prefix("install_version_id:")
                        .filter(|id| legacy_version_id(id) && source_version_id(id))
                        .ok_or(HistoryError::Invalid)?;
                    if !session_id(&session.id, "install") || target.id != legacy_target_id(version)
                    {
                        return Err(HistoryError::Invalid);
                    }
                    Identity::Vanilla(version.to_owned())
                }
                [kind, version, component, build] if kind == "install_kind:loader" => {
                    let version = version
                        .strip_prefix("install_version_id:")
                        .ok_or(HistoryError::Invalid)?;
                    let component = component
                        .strip_prefix("loader_component:")
                        .and_then(LoaderComponentId::parse)
                        .ok_or(HistoryError::Invalid)?;
                    let (actual, base, loader) = build
                        .strip_prefix("loader_build_id:")
                        .and_then(parse_build_id)
                        .ok_or(HistoryError::Invalid)?;
                    let canonical = installed_version_id_for(component, &base, &loader)
                        .map_err(|_| HistoryError::Invalid)?;
                    if component != actual
                        || version != canonical
                        || !source_version_id(&base)
                        || target.id != legacy_target_id(version)
                        || !session_id(&session.id, "loader-install")
                    {
                        return Err(HistoryError::Invalid);
                    }
                    Identity::Loader {
                        version: canonical,
                        base,
                    }
                }
                _ => return Err(HistoryError::Invalid),
            }
        }
        "ModifyInstanceContent"
            if planned.step_id == "modify_instance_content"
                && planned.generated_facts.is_empty() =>
        {
            let target = source
                .targets
                .iter()
                .find(|target| target.kind == "Instance")
                .ok_or(HistoryError::Invalid)?;
            if !lower_hex(&target.id, 16) || !session_id(&session.id, "content") {
                return Err(HistoryError::Invalid);
            }
            Identity::Content(target.id.clone())
        }
        _ => return Err(HistoryError::Invalid),
    };
    let failed = match (
        &identity,
        source.status.as_str(),
        source.outcome.as_deref(),
        source.failure_point.as_deref(),
    ) {
        (
            Identity::Content(id),
            "Failed",
            Some("Failed"),
            Some("content_initialization_cancelled"),
        ) => {
            if !source.guardian_diagnosis_ids.is_empty()
                || source.guardian_install_terminal.is_some()
            {
                return Err(HistoryError::Invalid);
            }
            let [step] = source.completed_steps.as_slice() else {
                return Err(HistoryError::Invalid);
            };
            if step.step_id != "content_progress_initializing"
                || step.phase != "Failed"
                || step.result != "Failed"
                || step.changed_target.is_some()
                || step.generated_facts
                    != [
                        "install_phase:initializing",
                        "install_done:true",
                        "install_error:true",
                    ]
                || step.rollback != "NotApplicable"
                || !step.guardian_fact_ids.is_empty()
                || step.metrics.is_some()
            {
                return Err(HistoryError::Invalid);
            }
            return Ok(Some(id.clone()));
        }
        (_, "Succeeded", Some("Succeeded"), None)
            if source.guardian_diagnosis_ids.is_empty()
                && source.guardian_install_terminal.is_none() =>
        {
            false
        }
        (
            Identity::Vanilla(_) | Identity::Loader { .. },
            "Failed",
            Some("Failed"),
            Some("install_progress_error"),
        ) => {
            validate_guardian_terminal(source)?;
            true
        }
        _ => return Err(HistoryError::Invalid),
    };
    let namespace = if matches!(&identity, Identity::Content(_)) {
        "content"
    } else {
        "install"
    };
    let mut seen = BTreeSet::new();
    let mut checkpoints = 0usize;
    let mut rolled_back = false;
    let mut recovering_seen = false;
    for (index, step) in source.completed_steps.iter().enumerate() {
        if !seen.insert(&step.step_id)
            || !structured_token(&step.step_id, 96)
            || (!(failed && index + 1 == source.completed_steps.len())
                && !step.guardian_fact_ids.is_empty())
        {
            return Err(HistoryError::Invalid);
        }
        if index + 1 == source.completed_steps.len() {
            if failed {
                validate_failed_progress(step)?;
            } else {
                validate_progress(step, namespace, true)?;
            }
            continue;
        }
        if rolled_back {
            return Err(HistoryError::Invalid);
        }
        if step.step_id.starts_with(&format!("{namespace}_progress_")) {
            validate_progress(step, namespace, false)?;
            recovering_seen |= step.step_id == "install_progress_recovering";
            continue;
        }
        let (kind, version) = match (&identity, checkpoints, failed) {
            (Identity::Vanilla(version), 0, false) => ("committed", version),
            (Identity::Loader { base, .. }, 0, false) => ("base_committed", base),
            (Identity::Loader { version, .. }, 1, false) => ("child_committed", version),
            (Identity::Vanilla(version), 0, true) => ("rolled_back", version),
            (Identity::Loader { base, .. }, 0, true)
                if step.step_id == "install_publication_rolled_back" =>
            {
                ("rolled_back", base)
            }
            (Identity::Loader { base, .. }, 0, true) => ("base_committed", base),
            (Identity::Loader { version, .. }, 1, true) => ("rolled_back", version),
            _ => return Err(HistoryError::Invalid),
        };
        if !recovering_seen {
            return Err(HistoryError::Invalid);
        }
        validate_checkpoint(step, kind, version)?;
        rolled_back = kind == "rolled_back";
        checkpoints += 1;
    }
    match identity {
        _ if failed && !rolled_back => Err(HistoryError::Invalid),
        Identity::Vanilla(_) if checkpoints == 1 => Ok(None),
        Identity::Loader { .. } if checkpoints == 2 || (failed && checkpoints == 1) => Ok(None),
        Identity::Content(id) if checkpoints == 0 => Ok(Some(id)),
        _ => Err(HistoryError::Invalid),
    }
}

fn validate_progress(
    step: &SourceStep,
    namespace: &str,
    terminal: bool,
) -> Result<(), HistoryError> {
    let prefix = format!("{namespace}_progress_");
    let phase = step
        .step_id
        .strip_prefix(&prefix)
        .filter(|phase| canonical_phase(phase))
        .ok_or(HistoryError::Invalid)?;
    if !plain_progress_fact(phase) {
        return Err(HistoryError::Invalid);
    }
    let expected_facts = if terminal {
        vec![
            "install_phase:done".to_owned(),
            "install_done:true".to_owned(),
        ]
    } else {
        vec![format!("install_phase:{phase}")]
    };
    let expected_phase = if terminal && namespace == "content" {
        "Downloading"
    } else if terminal {
        "Completed"
    } else {
        operation_phase(phase)
    };
    if step.result != "Completed"
        || step.rollback != "NotApplicable"
        || step.changed_target.is_some()
        || step.generated_facts != expected_facts
        || step.phase != expected_phase
        || (terminal && phase != "done")
        || (step.metrics.is_some() != (terminal && namespace == "content"))
    {
        return Err(HistoryError::Invalid);
    }
    Ok(())
}

fn validate_failed_progress(step: &SourceStep) -> Result<(), HistoryError> {
    if step.step_id != "install_progress_error"
        || step.phase != "Failed"
        || step.result != "Failed"
        || step.changed_target.is_some()
        || step.generated_facts
            != [
                "install_phase:error",
                "install_done:true",
                "install_error:true",
            ]
        || step.rollback != "NotApplicable"
        || step.metrics.is_some()
        || !guardian_labels(&step.guardian_fact_ids, 64, GUARDIAN_FACT_IDS)
    {
        return Err(HistoryError::Invalid);
    }
    Ok(())
}

fn validate_guardian_terminal(source: &SourceOperation) -> Result<(), HistoryError> {
    if !guardian_labels(&source.guardian_diagnosis_ids, 32, GUARDIAN_DIAGNOSIS_IDS) {
        return Err(HistoryError::Invalid);
    }
    let Some(terminal) = &source.guardian_install_terminal else {
        return Ok(());
    };
    if !source
        .guardian_diagnosis_ids
        .contains(&terminal.diagnosis_id)
        || !matches!(
            terminal.action.as_str(),
            "Allow"
                | "Warn"
                | "Repair"
                | "Retry"
                | "Strip"
                | "Downgrade"
                | "Fallback"
                | "Quarantine"
                | "AskUser"
                | "Block"
                | "RecordOnly"
        )
        || (terminal.action == "Retry") != terminal.memory.is_some()
    {
        return Err(HistoryError::Invalid);
    }
    if let Some(memory) = &terminal.memory {
        let target = &memory.target;
        let timestamp = |value: &str| {
            if value.len() > 40 {
                return None;
            }
            let parsed = chrono::DateTime::parse_from_rfc3339(value)
                .ok()?
                .with_timezone(&chrono::Utc);
            (parsed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) == value).then_some(parsed)
        };
        let observed = timestamp(&memory.observed_at).ok_or(HistoryError::Invalid)?;
        let suppression = timestamp(&memory.suppression_until).ok_or(HistoryError::Invalid)?;
        if !lower_hex(&memory.binding, 64)
            || target.system != "Execution"
            || target.kind != "Artifact"
            || !matches!(
                target.ownership.as_str(),
                "LauncherManaged" | "ExternalProviderDerived"
            )
            || !structured_token(&target.id, 96)
            || legacy_target_id(&target.id) != target.id
            || observed.checked_add_signed(chrono::Duration::minutes(5)) != Some(suppression)
        {
            return Err(HistoryError::Invalid);
        }
    }
    Ok(())
}

fn guardian_labels(values: &[String], maximum: usize, known: &[&str]) -> bool {
    values.len() <= maximum
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
        && values.iter().all(|value| known.contains(&value.as_str()))
}

// Closed predecessor wire registries, retained only to validate excluded data.
const GUARDIAN_FACT_IDS: &[&str] = &[
    "agent_hook_failed",
    "agent_unavailable",
    "artifact_checksum_mismatch",
    "artifact_hash_mismatch",
    "artifact_missing",
    "artifact_quarantined",
    "artifact_size_drift",
    "artifact_size_mismatch",
    "asset_index_missing",
    "atomic_promotion_completed",
    "atomic_promotion_failed",
    "auth_mode_incompatible",
    "boot_marker_observed",
    "boot_milestone_overdue",
    "boot_milestone_reached",
    "classpath_module_conflict",
    "client_jar_missing",
    "custom_java_override_present",
    "custom_jvm_args_present",
    "custom_jvm_preset_present",
    "download_interrupted",
    "download_provider_unavailable",
    "download_temp_discarded",
    "download_written_to_temp",
    "exit_code_nonzero",
    "exit_code_unknown",
    "exit_code_zero",
    "filesystem_permission_denied",
    "frame_budget_exceeded",
    "gc_pause_storm",
    "graphics_driver_crash",
    "heap_pressure_critical",
    "incomplete_install",
    "install_dependency_failed",
    "install_execution_failed",
    "install_processor_failed",
    "installed_versions_degraded",
    "java_major_mismatch",
    "java_override_empty",
    "java_override_missing",
    "java_override_undefined_sentinel",
    "java_probe_failed",
    "java_update_too_old",
    "jvm_arg_agent_override",
    "jvm_arg_experimental_unlock_missing",
    "jvm_arg_memory_conflict",
    "jvm_arg_reserved_launcher_flag",
    "jvm_arg_unlock_order_invalid",
    "jvm_arg_unsafe_classpath_override",
    "jvm_arg_unsafe_native_path_override",
    "jvm_arg_unsupported",
    "jvm_arg_unsupported_gc",
    "jvm_args_empty",
    "jvm_args_parse_failed",
    "jvm_preset_compatibility_adjusted",
    "launch_failure_classified",
    "launch_jvm_preset_downgrade_available",
    "launch_jvm_strip_available",
    "launch_memory_allocation_low",
    "launch_memory_min_clamped",
    "launch_resource_cpu_pressure",
    "launch_resource_disk_pressure",
    "launch_resource_install_pressure",
    "launch_resource_memory_pressure",
    "launch_runtime_fallback_available",
    "launcher_managed_artifact_signature_corruption",
    "launcher_stop_requested",
    "libraries_missing",
    "loader_bootstrap_failure",
    "managed_runtime_corrupt",
    "managed_runtime_missing",
    "managed_runtime_ready_marker_missing",
    "managed_runtime_repair_applied",
    "managed_runtime_rosetta_required",
    "managed_runtime_unavailable_for_platform",
    "missing_dependency",
    "mod_attributed_crash",
    "mod_transformation_failure",
    "no_structured_fact_startup",
    "no_structured_fact_planning",
    "no_structured_fact_validating",
    "no_structured_fact_downloading",
    "no_structured_fact_installing",
    "no_structured_fact_preparing",
    "no_structured_fact_launching",
    "no_structured_fact_running",
    "no_structured_fact_repairing",
    "no_structured_fact_rolling_back",
    "no_structured_fact_completed",
    "no_structured_fact_failed",
    "out_of_memory",
    "parent_version_missing",
    "performance_fallback_selected",
    "performance_health_invalid",
    "performance_rules_invalid",
    "performance_user_owned_conflict",
    "persisted_state_repair_available",
    "persisted_state_schema_invalid",
    "primitive_refused",
    "process_exited",
    "process_exited_after_boot",
    "process_exited_before_boot",
    "process_killed",
    "process_spawned",
    "provider_data_invalid",
    "recent_repair_failed",
    "recent_startup_failure",
    "registered_artifact_repair_available",
    "registered_component_rebuild_failed",
    "repair_suppressed_until",
    "startup_window_expired",
    "temp_file_write_failed",
    "unknown_launch_failure",
    "user_mod_set_drift",
    "version_json_missing",
    "watchdog_action_observed",
    "watchdog_killed_process",
];

const GUARDIAN_DIAGNOSIS_IDS: &[&str] = &[
    "artifact_ownership_unsafe",
    "atomic_promotion_failed",
    "download_unavailable",
    "filesystem_permission_denied",
    "install_artifact_metadata_invalid",
    "install_dependency_failed",
    "install_execution_failed",
    "install_processor_failed",
    "java_override_unavailable",
    "java_probe_failed",
    "java_runtime_major_mismatch",
    "java_runtime_update_too_old",
    "jvm_arg_unsafe_override",
    "jvm_arg_unsupported",
    "jvm_args_empty",
    "jvm_args_malformed",
    "launcher_managed_artifact_corrupt",
    "launcher_managed_artifact_signature_corrupt",
    "managed_runtime_corrupt",
    "managed_runtime_missing",
    "managed_runtime_rosetta_required",
    "managed_runtime_unavailable_for_platform",
    "performance_fallback_selected",
    "performance_rules_invalid",
    "performance_user_owned_conflict",
    "persisted_state_schema_invalid",
    "process_lifecycle_observed",
    "temp_file_write_failed",
    "installed_version_metadata_missing",
    "parent_version_metadata_missing",
    "install_incomplete",
    "client_jar_missing",
    "libraries_missing",
    "asset_index_missing",
    "launch_memory_min_clamped",
    "launch_memory_allocation_low",
    "launch_resource_memory_pressure",
    "launch_resource_cpu_pressure",
    "launch_resource_install_pressure",
    "launch_resource_disk_pressure",
    "custom_java_override_present",
    "custom_jvm_preset_present",
    "custom_jvm_args_present",
    "performance_health_invalid",
    "jvm_preset_adjusted",
    "launch_prepare_failed",
    "startup_stalled",
    "out_of_memory",
    "graphics_driver_crash",
    "missing_dependency",
    "mod_transformation_failure",
    "mod_attributed_crash",
    "classpath_module_conflict",
    "auth_mode_incompatible",
    "loader_bootstrap_failure",
    "startup_failed_unknown",
    "java_runtime_recovery",
    "jvm_preset_recovery",
    "unknown",
    "jvm_unsupported_option",
    "jvm_experimental_unlock",
    "jvm_option_ordering",
    "java_runtime_mismatch",
    "launcher_managed_artifact_signature",
    "unknown_failure_startup",
    "unknown_failure_planning",
    "unknown_failure_validating",
    "unknown_failure_downloading",
    "unknown_failure_installing",
    "unknown_failure_preparing",
    "unknown_failure_launching",
    "unknown_failure_running",
    "unknown_failure_repairing",
    "unknown_failure_rolling_back",
    "unknown_failure_completed",
    "unknown_failure_failed",
];

fn validate_checkpoint(step: &SourceStep, kind: &str, version: &str) -> Result<(), HistoryError> {
    let expected_step = match kind {
        "committed" => "install_publication_committed",
        "base_committed" => "install_base_publication_committed",
        "child_committed" => "install_child_publication_committed",
        "rolled_back" => "install_publication_rolled_back",
        _ => return Err(HistoryError::Invalid),
    };
    let rolled_back = kind == "rolled_back";
    let (publication, recorded_version, evidence) = match step.generated_facts.as_slice() {
        [publication, version, evidence] if rolled_back => (publication, version, evidence),
        [publication, version, evidence, contract]
            if !rolled_back
                && contract
                    .strip_prefix("install_activation_contract:")
                    .and_then(|id| ManagedInstallActivationContractId::parse(id).ok())
                    .is_some() =>
        {
            (publication, version, evidence)
        }
        _ => return Err(HistoryError::Invalid),
    };
    let evidence = evidence
        .strip_prefix("install_publication_evidence:")
        .and_then(|id| ManagedInstallPublicationEvidenceId::parse(id).ok())
        .ok_or(HistoryError::Invalid)?;
    if step.step_id != expected_step
        || step.phase
            != if rolled_back {
                "RollingBack"
            } else {
                "Installing"
            }
        || step.rollback
            != if rolled_back {
                "Applied"
            } else {
                "NotApplicable"
            }
        || step.result != "Completed"
        || step.metrics.is_some()
        || publication != &format!("install_publication:{kind}")
        || recorded_version != &format!("install_publication_version_id:{version}")
        || !evidence.matches_version_id(version)
        || step.changed_target.as_ref().is_none_or(|target| {
            target.system != "Application"
                || target.kind != "Version"
                || target.ownership != "LauncherManaged"
                || !structured_token(&target.id, 96)
                || target.id != legacy_target_id(version)
        })
    {
        return Err(HistoryError::Invalid);
    }
    Ok(())
}

fn session_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('-'))
        .is_some_and(|value| lower_hex(value, 32))
}

fn legacy_version_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

fn source_version_id(value: &str) -> bool {
    if value.starts_with("loader-v2-") {
        axial_minecraft::is_canonical_installed_loader_id(value)
    } else {
        legacy_version_id(value)
    }
}

fn canonical_phase(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b':')
        })
        && !legacy_sensitive(value)
}

fn structured_token(value: &str, maximum: usize) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.is_empty()
        || value.len() > maximum
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b':')
        })
        || lower.starts_with("-d")
        || [
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
            "authorization",
            "credential",
            "bearer",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return false;
    }
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() >= 3
        && parts.iter().take(3).all(|part| {
            part.len() >= 12
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return false;
    }
    // Unlike the producer's text redaction, the persisted structured-token
    // contract splits secret-like runs at underscores and hyphens too.
    !value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| {
            part.len() >= 48
                && part.bytes().any(|byte| byte.is_ascii_alphabetic())
                && part.bytes().any(|byte| byte.is_ascii_digit())
        })
}

fn plain_progress_fact(phase: &str) -> bool {
    // Legacy generated facts select these codecs by substring, then require an
    // exact prefix. A phase cannot smuggle one inside its install_phase fact.
    ![
        "install_publication_evidence:",
        "install_activation_contract:",
        "install_publication_version_id:",
        "install_version_id:",
        "loader_build_id:",
        "performance_plan_graph_sha512_",
    ]
    .iter()
    .any(|prefix| phase.contains(prefix))
}

fn operation_phase(value: &str) -> &'static str {
    match value {
        "version_json" | "client_jar" | "libraries" | "asset_index" | "assets" | "log_config"
        | "java_runtime" | "java_runtime_ready" | "artifacts" | "loader_libraries" | "download" => {
            "Downloading"
        }
        "profile" | "processors" | "loader_overlay" | "loader_publish" | "overrides" | "commit"
        | "removing" => "Installing",
        "planning" => "Planning",
        "recovering" => "Repairing",
        _ => "Running",
    }
}

// Source TargetDescriptor::new redacts before its 96-character projection. In
// particular, opaque loader IDs can legitimately have the recorded ID "target".
fn legacy_target_id(value: &str) -> String {
    if legacy_sensitive(value) {
        return "target".into();
    }
    let target: String = value
        .chars()
        .take(96)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let target = target.trim_matches('_');
    if target.is_empty() {
        "target".into()
    } else {
        target.into()
    }
}

// The admitted version/phase alphabets are ASCII tokens. These are the original
// producer's sensitivity rules restricted to those alphabets, not a new filter.
fn legacy_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("-d")
        || [
            ".jar",
            ".exe",
            ".dll",
            ".dylib",
            ".so",
            "-x",
            "--",
            "token",
            "secret",
            "password",
            "provider_payload",
            "account_id",
            "authorization",
            "credential",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return true;
    }
    let token = value
        .trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')));
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() >= 3
        && parts.iter().take(3).all(|part| {
            part.len() >= 12
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return true;
    }
    value
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
        .any(|part| {
            part.len() >= 48
                && part.bytes().any(|byte| byte.is_ascii_alphabetic())
                && part.bytes().any(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MetadataStore;
    use serde_json::{Value, json};

    const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_SOURCE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn source_records() -> Vec<SourceOperation> {
        let journal = crate::import::tests::successful_install_journal();
        serde_json::from_value(journal["entries"].clone()).unwrap()
    }

    fn cancelled_initialization() -> SourceOperation {
        let journal = crate::import::tests::cancelled_content_initialization_journal();
        serde_json::from_value(journal["entries"][0].clone()).unwrap()
    }

    fn rolled_back_records() -> Vec<SourceOperation> {
        let journal = crate::import::tests::rolled_back_install_journal();
        serde_json::from_value(journal["entries"].clone()).unwrap()
    }

    fn prepare(records: Vec<SourceOperation>) -> Vec<PreparedOperation> {
        records
            .into_iter()
            .map(|record| PreparedOperation::prepare(SOURCE, record).unwrap())
            .collect()
    }

    fn global_records(count: u64) -> Vec<PreparedOperation> {
        let original = source_records().remove(0);
        (1..=count)
            .map(|number| {
                let mut source = original.clone();
                source.operation_id = format!("op-10000000-0000-4000-8000-{number:012x}");
                source.journal_id = format!("journal-{}", source.operation_id);
                source.sequence = number;
                source.targets[0].id = format!("install-{number:032x}");
                PreparedOperation::prepare(SOURCE, source).unwrap()
            })
            .collect()
    }

    fn archived_content_records(count: u64) -> Vec<PreparedOperation> {
        let original = source_records().remove(2);
        (1..=count)
            .map(|number| {
                let mut source = original.clone();
                source.operation_id = format!("op-20000000-0000-4000-8000-{number:012x}");
                source.journal_id = format!("journal-{}", source.operation_id);
                source.sequence = 1000 + number;
                source.targets[0].id = format!("content-{number:032x}");
                PreparedOperation::prepare(SOURCE, source).unwrap()
            })
            .collect()
    }

    fn fixture() -> (MetadataStore, InstanceId, String, Vec<PreparedOperation>) {
        let store = MetadataStore::in_memory().unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let records = prepare(source_records());
        let legacy = records[2].legacy_instance_id().unwrap().to_owned();
        (store, InstanceId::new(), legacy, records)
    }

    fn read(
        store: &MetadataStore,
        legacy: &str,
        instance: &InstanceId,
        after: Option<&str>,
    ) -> Result<HistoryPage, HistoryError> {
        store.read(|db| read_in(db, SOURCE, legacy, instance, after))
    }

    fn count(store: &MetadataStore) -> i64 {
        store
            .read(|db| {
                db.query_row("SELECT count(*) FROM install_history", [], |row| row.get(0))
                    .map_err(StorageError::from)
            })
            .unwrap()
    }

    fn padded_snapshot(
        store: &MetadataStore,
        batch: &PreparedImport,
        scope: CompletionScope,
    ) -> CompletionProof {
        store
            .transaction(|tx| -> Result<(), StorageError> {
                for record in &batch.0 {
                    let mut bytes = record.bytes.clone();
                    bytes.resize(MAX_RECORD_BYTES, b' ');
                    tx.execute(
                        "UPDATE install_history SET payload=?1 WHERE id=?2",
                        params![bytes, record.operation.0.id],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let mut proof = batch.proof(SOURCE, scope).unwrap();
        let mut digest = completion_hash(SOURCE, &proof.ids, scope);
        for id in &proof.ids {
            let (_, _, bytes) = store
                .read(|db| stored_row(db, id, MAX_RECORD_BYTES))
                .unwrap()
                .unwrap();
            let bytes = bytes.unwrap();
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(&bytes);
        }
        proof.digest = hex::encode(digest.finalize());
        proof
    }

    #[test]
    fn archived_content_history_readback_preserves_supported_terminal_records() {
        let mut succeeded = source_records().remove(2);
        succeeded.sequence = u64::MAX - 1;
        let SourceMetrics::ContentDownload(metrics) = succeeded
            .completed_steps
            .last_mut()
            .unwrap()
            .metrics
            .as_mut()
            .unwrap();
        metrics.written_to_temp = u64::MAX;
        metrics.promoted = u64::MAX;
        for source in [succeeded, cancelled_initialization()] {
            let operation = PreparedOperation::prepare(SOURCE, source.clone()).unwrap();
            let instance = format!(
                "archived-{SOURCE}-{}",
                operation.legacy_instance_id().unwrap()
            );
            assert_eq!(instance.len(), 90);
            assert!(instance.parse::<InstanceId>().is_err());
            let bytes = serde_json::to_vec(&json!({
                "schema": 1,
                "id": operation.0.id,
                "source_id": SOURCE,
                "instance_id": instance,
                "source": source,
            }))
            .unwrap();
            let record = decode_stored(&operation.0.id, SOURCE, Some(&instance), Some(bytes))
                .expect("supported archived content history must decode")
                .project();
            assert_eq!(record.instance_id.as_deref(), Some(instance.as_str()));
            assert_eq!(record.operation_id, source.operation_id);
            assert_eq!(record.journal_id, source.journal_id);
            assert_eq!(record.sequence, source.sequence.to_string());
            assert_eq!(record.command, "ModifyInstanceContent");
            assert_eq!(record.outcome, source.outcome.unwrap());
            assert_eq!(record.failure_point, source.failure_point);
            assert_eq!(
                serde_json::to_value(&record.targets).unwrap(),
                serde_json::to_value(&source.targets).unwrap()
            );
            let wire = serde_json::to_value(&record).unwrap();
            for field in ["accepted_at", "created_at", "allowed_actions", "status"] {
                assert!(wire.get(field).is_none(), "{field}");
            }
            let terminal = wire["completed_steps"].as_array().unwrap().last().unwrap();
            if record.outcome == "Succeeded" {
                assert!(wire.get("failure_point").is_none());
                assert_eq!(terminal["phase"], "Downloading");
                assert_eq!(
                    terminal["generated_facts"],
                    json!(["install_phase:done", "install_done:true"])
                );
                let counters = terminal["metrics"]["values"].as_object().unwrap();
                assert_eq!(counters.len(), 13);
                assert!(counters.values().all(Value::is_string));
                assert_eq!(counters["written_to_temp"], u64::MAX.to_string());
                assert_eq!(counters["promoted"], u64::MAX.to_string());
            } else {
                assert_eq!(wire["failure_point"], "content_initialization_cancelled");
                assert_eq!(record.completed_steps.len(), 1);
                assert_eq!(terminal["step_id"], "content_progress_initializing");
                assert_eq!(terminal["phase"], "Failed");
                assert!(terminal["metrics"].is_null());
                assert!(terminal["changed_target"].is_null());
            }
        }
    }

    #[test]
    fn archived_content_history_binding_shares_exact_bytes_in_either_publication_order() {
        for metadata_first in [false, true] {
            let (store, instance, legacy, records) = fixture();
            let archived =
                PreparedImport::bind_archived(SOURCE, archived_content_records(2)).unwrap();
            let proof = archived.archived_completion_proof(SOURCE).unwrap();
            let mut combined = PreparedImport::bind(records, &legacy, &instance).unwrap();
            combined.append(&archived).unwrap();
            assert!(Arc::ptr_eq(&combined.0[3], &archived.0[0]));
            assert!(Arc::ptr_eq(&combined.0[4], &archived.0[1]));
            for batch in if metadata_first {
                [&archived, &combined]
            } else {
                [&combined, &archived]
            } {
                store.transaction(|tx| batch.insert_in(tx)).unwrap();
            }
            store.read(|db| combined.verify_in(db)).unwrap();
            store
                .read(|db| read_completed_in(db, SOURCE, None, Some(&proof), None))
                .unwrap();
            assert_eq!(count(&store), 5);
            let live = read(&store, &legacy, &instance, None).unwrap();
            assert_eq!(live.records.len(), 3);
            assert!(live.records.iter().all(|record| {
                record
                    .instance_id
                    .as_deref()
                    .is_none_or(|id| id == instance.as_str())
            }));
            let page = store
                .read(|db| read_completed_in(db, SOURCE, None, Some(&proof), None))
                .unwrap();
            assert_eq!(page.records.len(), 2);
            assert!(page.records.iter().all(|record| {
                record.instance_id.as_deref()
                    == Some(format!("archived-{SOURCE}-{legacy}").as_str())
            }));
            for record in &archived.0 {
                let stored = store
                    .read(|db| stored_row(db, &record.operation.0.id, MAX_RECORD_BYTES))
                    .unwrap()
                    .unwrap();
                assert_eq!(stored.2.as_deref(), Some(record.bytes.as_slice()));
                assert_eq!(record.bytes.len(), record.operation.0.stored_bytes + 54);
            }
        }
    }

    #[test]
    fn archived_content_history_rejects_scope_source_identity_and_duplicate_conflicts() {
        let (store, instance, legacy, records) = fixture();
        let archived_records = archived_content_records(2);
        assert!(PreparedImport::bind_archived("invalid", vec![]).is_err());
        assert!(PreparedImport::bind_archived(OTHER_SOURCE, archived_records.clone()).is_err());
        assert!(PreparedImport::bind_archived(SOURCE, global_records(1)).is_err());
        assert!(
            PreparedImport::bind_archived(SOURCE, vec![archived_records[0].clone(); 2]).is_err()
        );
        assert!(PreparedImport::bind_archived(SOURCE, archived_content_records(129)).is_err());
        let archived = PreparedImport::bind_archived(SOURCE, archived_records.clone()).unwrap();
        assert!(archived.completion_proof(SOURCE).is_err());
        assert!(archived.archived_completion_proof(OTHER_SOURCE).is_err());
        let live = PreparedImport::bind(records, &legacy, &instance).unwrap();
        assert!(live.archived_completion_proof(SOURCE).is_err());
        let mut duplicate = PreparedImport::bind_archived(SOURCE, archived_records).unwrap();
        assert!(duplicate.append(&archived).is_err());
        assert_eq!(duplicate.0.len(), 2);

        let global = PreparedImport::bind_global(vec![]).unwrap();
        let old = global.completion_proof(SOURCE).unwrap();
        let empty = PreparedImport::bind_archived(SOURCE, vec![]).unwrap();
        let archive_proof = empty.archived_completion_proof(SOURCE).unwrap();
        assert_eq!(archive_proof.count(), 0);
        assert_ne!(old.digest, archive_proof.digest);
        assert!(
            store
                .read(|db| read_completed_in(db, SOURCE, None, Some(&old), None))
                .is_err()
        );
        assert!(
            store
                .read(|db| archive_proof.verify_in(db, SOURCE))
                .is_err()
        );
        assert!(
            store
                .read(|db| read_completed_in(db, SOURCE, None, None, None))
                .is_err()
        );
        let page = store
            .read(|db| read_completed_in(db, SOURCE, Some(&old), Some(&archive_proof), None))
            .unwrap();
        assert!(page.records.is_empty());
        assert!(page.next_after.is_none());
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            format!(
                "{{\"source_id\":\"{SOURCE}\",\"ids\":[],\"digest\":\"7cf39a7e06414d5ce13f50e49836d6051d05720096e709581d7302f681c3c81b\"}}"
            )
        );

        let full = archived_content_records(64);
        let mut combined = PreparedImport::bind_global(global_records(64)).unwrap();
        let first = PreparedImport::bind_archived(SOURCE, full).unwrap();
        PreparedImport::validate_with_archived(&global_records(64), &first).unwrap();
        combined.append(&first).unwrap();
        let extra = PreparedImport::bind_archived(
            SOURCE,
            vec![archived_content_records(65).pop().unwrap()],
        )
        .unwrap();
        assert!(combined.append(&extra).is_err());
        assert_eq!(combined.0.len(), 128);
        assert!(PreparedImport::validate_with_archived(&global_records(65), &first).is_err());
        let conflicting = PreparedImport::bind_global(vec![
            PreparedOperation::prepare(SOURCE, {
                let mut source = source_records().remove(0);
                source.sequence = 1001;
                source
            })
            .unwrap(),
        ])
        .unwrap();
        assert!(duplicate.append(&conflicting).is_err());
        assert_eq!(duplicate.0.len(), 2);
    }

    #[test]
    fn archived_content_history_combined_receipt_is_immutable_paginated_and_source_free() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("history.sqlite");
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let global = PreparedImport::bind_global(global_records(20)).unwrap();
        let archived = PreparedImport::bind_archived(SOURCE, archived_content_records(20)).unwrap();
        let global_proof = global.completion_proof(SOURCE).unwrap();
        let archive_proof = archived.archived_completion_proof(SOURCE).unwrap();
        let encoded = serde_json::to_vec(&(global_proof.clone(), archive_proof.clone())).unwrap();
        store
            .transaction(|tx| {
                global.insert_in(tx)?;
                archived.insert_in(tx)
            })
            .unwrap();
        let first = store
            .read(|db| {
                read_completed_in(db, SOURCE, Some(&global_proof), Some(&archive_proof), None)
            })
            .unwrap();
        assert_eq!(first.records.len(), 32);
        assert!(first.next_after.is_some());
        let later = PreparedImport::bind_archived(
            SOURCE,
            vec![archived_content_records(21).pop().unwrap()],
        )
        .unwrap();
        store.transaction(|tx| later.insert_in(tx)).unwrap();
        drop(store);
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let (global_proof, archive_proof): (CompletionProof, CompletionProof) =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            first,
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&archive_proof),
                    None
                ))
                .unwrap()
        );
        let last = store
            .read(|db| {
                read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&archive_proof),
                    first.next_after.as_deref(),
                )
            })
            .unwrap();
        assert_eq!(last.records.len(), 8);
        assert!(last.next_after.is_none());
        let actual: Vec<_> = first
            .records
            .iter()
            .chain(&last.records)
            .map(|record| record.id.clone())
            .collect();
        let mut expected = global_proof.ids.clone();
        expected.extend(archive_proof.ids.iter().cloned());
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(count(&store), 41);
        let tail = actual.last().unwrap();
        store
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute("DELETE FROM install_history WHERE id=?1", [tail])?;
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&archive_proof),
                    None
                ))
                .is_err()
        );
        assert_eq!(count(&store), 40);
    }

    #[test]
    fn archived_content_history_replay_requires_original_encoded_tuple() {
        #[derive(Serialize)]
        struct PreviousStored<'a> {
            schema: u8,
            id: &'a str,
            source_id: &'a str,
            instance_id: Option<&'a InstanceId>,
            source: &'a SourceOperation,
        }
        let (store, instance, legacy, records) = fixture();
        let batch = PreparedImport::bind(records, &legacy, &instance).unwrap();
        store.transaction(|tx| -> Result<(), StorageError> {
            for record in &batch.0 {
                let bytes = serde_json::to_vec(&PreviousStored {
                    schema: 1, id: &record.operation.0.id, source_id: SOURCE,
                    instance_id: record.instance_id.as_ref().map(|_| &instance),
                    source: &record.operation.0.source,
                }).unwrap();
                assert_eq!(bytes, record.bytes);
                tx.execute("INSERT INTO install_history(id,source_id,instance_id,payload) VALUES(?1,?2,?3,?4)", params![
                    record.operation.0.id, SOURCE, record.instance_id, bytes,
                ])?;
            }
            Ok(())
        }).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.read(|db| batch.verify_in(db)).unwrap();
        let archived = PreparedImport::bind_archived(SOURCE, archived_content_records(1)).unwrap();
        store.transaction(|tx| archived.insert_in(tx)).unwrap();
        for record in [&batch.0[0], &archived.0[0]] {
            let mut changed = record.bytes.clone();
            changed.push(b' ');
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE install_history SET payload=?1 WHERE id=?2",
                        params![changed, record.operation.0.id],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        for prepared in [&batch, &archived] {
            assert!(matches!(
                store.transaction(|tx| prepared.insert_in(tx)),
                Err(HistoryError::Conflict)
            ));
            assert!(matches!(
                store.read(|db| prepared.verify_in(db)),
                Err(HistoryError::Conflict)
            ));
        }
        assert_eq!(count(&store), 4);
    }

    #[test]
    fn archived_content_history_combined_receipt_charges_actual_stored_bytes() {
        let (store, _, _, _) = fixture();
        let global = PreparedImport::bind_global(global_records(16)).unwrap();
        let archived = PreparedImport::bind_archived(SOURCE, archived_content_records(16)).unwrap();
        store
            .transaction(|tx| {
                global.insert_in(tx)?;
                archived.insert_in(tx)
            })
            .unwrap();
        let global_proof = padded_snapshot(&store, &global, CompletionScope::Global);
        let archive_proof = padded_snapshot(&store, &archived, CompletionScope::Archived);
        let page = store
            .read(|db| {
                read_completed_in(db, SOURCE, Some(&global_proof), Some(&archive_proof), None)
            })
            .unwrap();
        assert_eq!(page.records.len(), 32);
        assert!(page.next_after.is_none());
        let extra = PreparedImport::bind_archived(
            SOURCE,
            vec![archived_content_records(17).pop().unwrap()],
        )
        .unwrap();
        store.transaction(|tx| extra.insert_in(tx)).unwrap();
        let mut expanded = archived.clone();
        expanded.append(&extra).unwrap();
        let expanded_proof = padded_snapshot(&store, &expanded, CompletionScope::Archived);
        store.read(|db| global_proof.verify_in(db, SOURCE)).unwrap();
        store
            .read(|db| read_completed_in(db, SOURCE, None, Some(&expanded_proof), None))
            .unwrap();
        assert!(
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&expanded_proof),
                    None
                ))
                .is_err()
        );
        assert!(
            store
                .read(|db| stored_row(db, &extra.0[0].operation.0.id, 0))
                .unwrap()
                .unwrap()
                .2
                .is_none()
        );
        assert_eq!(count(&store), 33);
        assert_eq!(
            page,
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&archive_proof),
                    None
                ))
                .unwrap()
        );
    }

    #[test]
    fn archived_content_history_combined_receipt_bounds_count_and_sequence_across_scopes() {
        let (store, _, _, _) = fixture();
        let global = PreparedImport::bind_global(global_records(64)).unwrap();
        let archived = PreparedImport::bind_archived(SOURCE, archived_content_records(64)).unwrap();
        store
            .transaction(|tx| {
                global.insert_in(tx)?;
                archived.insert_in(tx)
            })
            .unwrap();
        let global_proof = global.completion_proof(SOURCE).unwrap();
        let archive_proof = archived.archived_completion_proof(SOURCE).unwrap();
        store
            .read(|db| {
                read_completed_in(db, SOURCE, Some(&global_proof), Some(&archive_proof), None)
            })
            .unwrap();
        let extra = PreparedImport::bind_archived(
            SOURCE,
            vec![archived_content_records(65).pop().unwrap()],
        )
        .unwrap();
        store.transaction(|tx| extra.insert_in(tx)).unwrap();
        let mut expanded = archived.clone();
        expanded.append(&extra).unwrap();
        let expanded_proof = expanded.archived_completion_proof(SOURCE).unwrap();
        store
            .read(|db| read_completed_in(db, SOURCE, None, Some(&expanded_proof), None))
            .unwrap();
        assert!(
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&expanded_proof),
                    None
                ))
                .is_err()
        );
        assert_eq!(count(&store), 129);

        let (store, _, _, _) = fixture();
        let global = PreparedImport::bind_global(global_records(1)).unwrap();
        let mut content = source_records().remove(2);
        content.sequence = 1;
        let archived = PreparedImport::bind_archived(SOURCE, prepare(vec![content])).unwrap();
        store
            .transaction(|tx| {
                global.insert_in(tx)?;
                archived.insert_in(tx)
            })
            .unwrap();
        let global_proof = global.completion_proof(SOURCE).unwrap();
        let archive_proof = archived.archived_completion_proof(SOURCE).unwrap();
        store.read(|db| global_proof.verify_in(db, SOURCE)).unwrap();
        store
            .read(|db| read_completed_in(db, SOURCE, None, Some(&archive_proof), None))
            .unwrap();
        assert!(
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&archive_proof),
                    None
                ))
                .is_err()
        );
        let mut duplicate = archive_proof;
        duplicate.ids = global_proof.ids.clone();
        assert!(
            store
                .read(|db| read_completed_in(
                    db,
                    SOURCE,
                    Some(&global_proof),
                    Some(&duplicate),
                    None
                ))
                .is_err()
        );
    }

    #[test]
    fn archived_content_history_readback_rejects_wrong_binding_and_unbounded_index_bytes() {
        for corruption in [
            "archive_source",
            "legacy",
            "indexed_binding",
            "source_bytes",
            "binding_bytes",
        ] {
            let (store, _, _, _) = fixture();
            let batch = PreparedImport::bind_archived(SOURCE, archived_content_records(1)).unwrap();
            let proof = batch.archived_completion_proof(SOURCE).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            let record = &batch.0[0];
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    let mut payload: Value = serde_json::from_slice(&record.bytes).unwrap();
                    match corruption {
                        "source_bytes" => {
                            tx.execute(
                                "UPDATE install_history SET source_id=?1 WHERE id=?2",
                                params!["é".repeat(64), record.operation.0.id],
                            )?;
                        }
                        "binding_bytes" => {
                            tx.execute(
                                "UPDATE install_history SET instance_id=?1 WHERE id=?2",
                                params!["é".repeat(46), record.operation.0.id],
                            )?;
                        }
                        "indexed_binding" => {
                            tx.execute(
                                "UPDATE install_history SET instance_id=?1 WHERE id=?2",
                                params![InstanceId::new().as_str(), record.operation.0.id],
                            )?;
                        }
                        _ => {
                            let instance = if corruption == "archive_source" {
                                format!(
                                    "archived-{OTHER_SOURCE}-{}",
                                    record.operation.legacy_instance_id().unwrap()
                                )
                            } else {
                                format!("archived-{SOURCE}-ffffffffffffffff")
                            };
                            payload["instance_id"] = json!(instance);
                            tx.execute(
                                "UPDATE install_history SET instance_id=?1,payload=?2 WHERE id=?3",
                                params![
                                    instance,
                                    serde_json::to_vec(&payload).unwrap(),
                                    record.operation.0.id
                                ],
                            )?;
                        }
                    }
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(
                    store.read(|db| read_completed_in(db, SOURCE, None, Some(&proof), None)),
                    Err(HistoryError::Conflict)
                ),
                "{corruption}"
            );
            assert!(
                matches!(
                    store.read(|db| batch.verify_in(db)),
                    Err(HistoryError::Conflict)
                ),
                "{corruption}"
            );
            assert!(
                matches!(
                    store.transaction(|tx| batch.insert_in(tx)),
                    Err(HistoryError::Conflict)
                ),
                "{corruption}"
            );
            if matches!(corruption, "source_bytes" | "binding_bytes") {
                assert!(
                    matches!(
                        store.read(|db| stored_row(db, &record.operation.0.id, MAX_RECORD_BYTES)),
                        Err(HistoryError::Conflict)
                    ),
                    "{corruption}"
                );
            }
            assert_eq!(count(&store), 1);
        }
    }

    #[test]
    fn archived_content_history_publication_rolls_back_ignored_or_byte_rewritten_rows() {
        for rewrite in [false, true] {
            let (store, _, _, _) = fixture();
            let batch = PreparedImport::bind_archived(SOURCE, archived_content_records(2)).unwrap();
            let first = &batch.0[0].operation.0.id;
            let second = &batch.0[1].operation.0.id;
            let trigger = if rewrite {
                format!(
                    "CREATE TRIGGER break_history AFTER INSERT ON install_history WHEN NEW.id='{second}' BEGIN UPDATE install_history SET payload=CAST(payload||' ' AS BLOB) WHERE id='{first}'; END;"
                )
            } else {
                format!(
                    "CREATE TRIGGER break_history BEFORE INSERT ON install_history WHEN NEW.id='{second}' BEGIN SELECT RAISE(IGNORE); END;"
                )
            };
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(&trigger)?;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(
                store.transaction(|tx| batch.insert_in(tx)),
                Err(HistoryError::Conflict)
            ));
            assert_eq!(count(&store), 0);
        }
    }

    #[test]
    fn successful_history_preserves_neutral_proof_and_lossless_public_numbers() {
        let (store, instance, legacy, _) = fixture();
        let mut source = source_records();
        source[2].sequence = u64::MAX - 1;
        let SourceMetrics::ContentDownload(metrics) = source[2]
            .completed_steps
            .last_mut()
            .unwrap()
            .metrics
            .as_mut()
            .unwrap();
        metrics.written_to_temp = u64::MAX;
        metrics.promoted = u64::MAX;
        let batch = PreparedImport::bind(prepare(source.clone()), &legacy, &instance).unwrap();
        assert!(
            batch
                .0
                .iter()
                .all(|record| record.bytes.len() == record.operation.0.stored_bytes)
        );
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.transaction(|tx| batch.verify_in(tx)).unwrap();
        let page = read(&store, &legacy, &instance, None).unwrap();
        assert_eq!(page.records.len(), 3);
        assert!(page.next_after.is_none());
        let encoded = serde_json::to_value(&page).unwrap();
        for original in &source {
            let saved = encoded["records"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["operation_id"] == original.operation_id)
                .unwrap();
            assert_eq!(saved["historical"], true);
            assert_eq!(saved["journal_id"], original.journal_id);
            assert_eq!(saved["sequence"], original.sequence.to_string());
            assert_eq!(saved["command"], original.command);
            assert_eq!(
                saved["targets"],
                serde_json::to_value(&original.targets).unwrap()
            );
            assert!(saved.get("status").is_none());
            assert!(saved.get("failure_point").is_none());
            assert!(saved.get("accepted_at").is_none());
            assert!(saved.get("created_at").is_none());
            assert!(saved.get("allowed_actions").is_none());
            let mut steps = serde_json::to_value(&original.completed_steps).unwrap();
            for step in steps.as_array_mut().unwrap() {
                step.as_object_mut().unwrap().remove("guardian_fact_ids");
                if let Some(values) = step
                    .get_mut("metrics")
                    .and_then(|metrics| metrics.get_mut("values"))
                    .and_then(Value::as_object_mut)
                {
                    for counter in values.values_mut() {
                        *counter = Value::String(counter.as_u64().unwrap().to_string());
                    }
                }
            }
            assert_eq!(saved["completed_steps"], steps);
            if original.command == "ModifyInstanceContent" {
                assert_eq!(saved["instance_id"], instance.as_str());
                let counters = saved["completed_steps"].as_array().unwrap().last().unwrap()["metrics"]["values"].as_object().unwrap();
                assert_eq!(counters.len(), 13);
                assert!(counters.values().all(Value::is_string));
                assert_eq!(counters["promoted"], u64::MAX.to_string());
                assert_eq!(counters["network_failure"], "1");
            } else {
                assert!(saved["instance_id"].is_null());
            }
        }
        assert_eq!(count(&store), 3);
    }

    #[test]
    fn successful_history_raw_codecs_reject_duplicate_unknown_and_nonnumeric_metrics() {
        let source = source_records().pop().unwrap();
        let raw = serde_json::to_string(&source).unwrap();
        assert!(serde_json::from_str::<SourceOperation>(&raw).is_ok());
        for replacement in [
            "\"promoted\":3,\"promoted\":4",
            "\"promoted\":3,\"unknown\":0",
            "\"promoted\":\"3\"",
            "\"promoted\":-1",
            "\"promoted\":3.0",
            "\"promoted\":18446744073709551616",
        ] {
            let changed = raw.replace("\"promoted\":3", replacement);
            assert_ne!(changed, raw);
            assert!(
                serde_json::from_str::<SourceOperation>(&changed).is_err(),
                "{replacement}"
            );
        }
        let duplicated_kind = raw.replace(
            "\"kind\":\"content_download\"",
            "\"kind\":\"content_download\",\"kind\":\"content_download\"",
        );
        assert!(serde_json::from_str::<SourceOperation>(&duplicated_kind).is_err());
        let nonstring_fact = raw.replace("\"install_phase:download\"", "123");
        assert!(serde_json::from_str::<SourceOperation>(&nonstring_fact).is_err());
    }

    #[test]
    fn successful_history_rejects_incomplete_or_contradictory_source_proofs() {
        let originals = source_records();
        for original in &originals {
            PreparedOperation::prepare(SOURCE, original.clone()).unwrap();
        }
        let mut invalid = Vec::new();
        for mutate in [
            |record: &mut SourceOperation| record.status = "Running".into(),
            |record: &mut SourceOperation| record.outcome = Some("Failed".into()),
            |record: &mut SourceOperation| {
                record.parent_operation_id = Some(record.operation_id.clone())
            },
            |record: &mut SourceOperation| record.rollback = "Available".into(),
            |record: &mut SourceOperation| record.owner = "Guardian".into(),
            |record: &mut SourceOperation| {
                record
                    .guardian_diagnosis_ids
                    .push("DownloadUnavailable".into())
            },
            |record: &mut SourceOperation| record.reconciliation_attempt = Some(json!({})),
            |record: &mut SourceOperation| record.reconciliation_terminal = Some(json!({})),
            |record: &mut SourceOperation| record.persisted_state_repair_attempt = Some(json!({})),
            |record: &mut SourceOperation| record.persisted_state_repair_terminal = Some(json!({})),
            |record: &mut SourceOperation| {
                record.guardian_install_terminal =
                    rolled_back_records()[0].guardian_install_terminal.clone()
            },
            |record: &mut SourceOperation| {
                record.planned_steps[0]
                    .generated_facts
                    .push("future_fact".into())
            },
            |record: &mut SourceOperation| {
                record
                    .completed_steps
                    .last_mut()
                    .unwrap()
                    .generated_facts
                    .push("install_error:true".into())
            },
            |record: &mut SourceOperation| {
                record.completed_steps.last_mut().unwrap().result = "Failed".into()
            },
            |record: &mut SourceOperation| {
                record.completed_steps[0]
                    .guardian_fact_ids
                    .push("DownloadUnavailable".into())
            },
        ] {
            let mut record = originals[0].clone();
            mutate(&mut record);
            invalid.push(record);
        }
        let mut vanilla = originals[0].clone();
        vanilla.completed_steps.remove(1);
        invalid.push(vanilla);
        let mut vanilla = originals[0].clone();
        vanilla.completed_steps.swap(0, 1);
        invalid.push(vanilla);
        let mut vanilla = originals[0].clone();
        vanilla.completed_steps[1].generated_facts[1] =
            "install_publication_version_id:another".into();
        invalid.push(vanilla);
        let mut vanilla = originals[0].clone();
        vanilla.completed_steps[1]
            .changed_target
            .as_mut()
            .unwrap()
            .id = "another".into();
        invalid.push(vanilla);
        let mut vanilla = originals[0].clone();
        vanilla.completed_steps[1].generated_facts[3] =
            "install_activation_contract:unknown".into();
        invalid.push(vanilla);
        let mut loader = originals[1].clone();
        loader.completed_steps.remove(2);
        invalid.push(loader);
        let mut loader = originals[1].clone();
        loader.completed_steps.swap(1, 2);
        invalid.push(loader);
        let mut loader = originals[1].clone();
        loader.planned_steps[0].generated_facts[2] =
            "loader_component:org.quiltmc.quilt-loader".into();
        invalid.push(loader);
        let mut content = originals[2].clone();
        content.completed_steps.last_mut().unwrap().metrics = None;
        invalid.push(content);
        let mut content = originals[2].clone();
        content.completed_steps.last_mut().unwrap().phase = "Completed".into();
        invalid.push(content);
        for (index, source) in invalid.into_iter().enumerate() {
            assert!(
                matches!(
                    PreparedOperation::prepare(SOURCE, source),
                    Err(HistoryError::Invalid)
                ),
                "mutation {index}"
            );
        }
    }

    #[test]
    fn successful_history_retains_original_structured_token_and_fact_codec_rules() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let original = source_records().remove(0);
        let mut ordinary = original.clone();
        let mut step = ordinary.completed_steps[0].clone();
        step.step_id = "install_progress_transferring".into();
        step.phase = "Running".into();
        step.generated_facts = vec!["install_phase:transferring".into()];
        ordinary.completed_steps.insert(0, step);
        PreparedOperation::prepare(SOURCE, ordinary.clone()).unwrap();
        let phase = format!("{}1", "a".repeat(31));
        let mut long_clean = ordinary.clone();
        long_clean.completed_steps[0].step_id = format!("install_progress_{phase}");
        long_clean.completed_steps[0].generated_facts = vec![format!("install_phase:{phase}")];
        PreparedOperation::prepare(SOURCE, long_clean).unwrap();
        for phase in [
            "bearer",
            "loader_build_id:abc",
            "install_version_id:abc",
            "performance_plan_graph_sha512_abcd",
        ] {
            let mut changed = ordinary.clone();
            changed.completed_steps[0].step_id = format!("install_progress_{phase}");
            changed.completed_steps[0].generated_facts = vec![format!("install_phase:{phase}")];
            assert!(
                PreparedOperation::prepare(SOURCE, changed).is_err(),
                "{phase}"
            );
        }
        let mut version = original;
        version.targets[1].id = "bearer".into();
        version.planned_steps[0].generated_facts[1] = "install_version_id:bearer".into();
        let checkpoint = &mut version.completed_steps[1];
        checkpoint.changed_target.as_mut().unwrap().id = "bearer".into();
        checkpoint.generated_facts[1] = "install_publication_version_id:bearer".into();
        let evidence = checkpoint.generated_facts[2]
            .strip_prefix("install_publication_evidence:")
            .unwrap();
        let mut parts: Vec<String> = evidence.split('.').map(str::to_owned).collect();
        parts[1] = URL_SAFE_NO_PAD.encode(Sha256::digest(b"bearer"));
        let evidence = parts.join(".");
        assert!(
            ManagedInstallPublicationEvidenceId::parse(&evidence)
                .unwrap()
                .matches_version_id("bearer")
        );
        checkpoint.generated_facts[2] = format!("install_publication_evidence:{evidence}");
        assert!(PreparedOperation::prepare(SOURCE, version).is_err());
    }

    #[test]
    fn successful_history_version_rows_are_global_and_content_binding_is_exact() {
        let (store, first, legacy, records) = fixture();
        let second = InstanceId::new();
        let other_legacy = "eeeeeeeeeeeeeeee";
        let first_batch = PreparedImport::bind(records.clone(), &legacy, &first).unwrap();
        store.transaction(|tx| first_batch.insert_in(tx)).unwrap();
        let globals = records
            .iter()
            .filter(|record| record.legacy_instance_id().is_none())
            .cloned()
            .collect();
        let second_batch = PreparedImport::bind(globals, other_legacy, &second).unwrap();
        store.transaction(|tx| second_batch.insert_in(tx)).unwrap();
        assert_eq!(count(&store), 3);
        let second_page = read(&store, other_legacy, &second, None).unwrap();
        assert_eq!(second_page.records.len(), 2);
        assert!(
            second_page
                .records
                .iter()
                .all(|record| record.instance_id.is_none())
        );
        assert!(PreparedImport::bind(records.clone(), other_legacy, &second).is_err());
        let wrong_destination = PreparedImport::bind(records, &legacy, &second).unwrap();
        assert!(matches!(
            store.transaction(|tx| wrong_destination.insert_in(tx)),
            Err(HistoryError::Conflict)
        ));
        assert_eq!(
            read(&store, &legacy, &first, None).unwrap().records.len(),
            3
        );
        assert!(read(&store, other_legacy, &first, None).is_err());
    }

    #[test]
    fn successful_history_ignored_and_rewritten_writes_rollback_the_entire_batch() {
        for trigger in [
            "CREATE TRIGGER break_history BEFORE INSERT ON install_history WHEN NEW.instance_id IS NOT NULL BEGIN SELECT RAISE(IGNORE); END;",
            "CREATE TRIGGER break_history AFTER INSERT ON install_history WHEN NEW.instance_id IS NOT NULL BEGIN UPDATE install_history SET payload=CAST('{}' AS BLOB) WHERE instance_id IS NULL; END;",
        ] {
            let (store, instance, legacy, records) = fixture();
            let batch = PreparedImport::bind(records, &legacy, &instance).unwrap();
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(trigger)?;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(
                store.transaction(|tx| batch.insert_in(tx)),
                Err(HistoryError::Conflict)
            ));
            assert_eq!(count(&store), 0);
        }
    }

    #[test]
    fn successful_history_readback_checks_semantics_identity_and_indexed_binding() {
        for corruption in [
            "step", "id", "source", "binding", "missing", "sequence", "legacy",
        ] {
            let (store, instance, legacy, records) = fixture();
            let batch = PreparedImport::bind(records, &legacy, &instance).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            let id = batch.0[2].operation.0.id.clone();
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    if corruption == "missing" {
                        tx.execute("DELETE FROM install_history WHERE id=?1", [&id])?;
                        return Ok(());
                    }
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT payload FROM install_history WHERE id=?1",
                        [&id],
                        |row| row.get(0),
                    )?;
                    let mut payload: Value = serde_json::from_slice(&bytes).unwrap();
                    match corruption {
                        "step" => {
                            payload["source"]["completed_steps"][0]["generated_facts"] =
                                json!(["future:effect"])
                        }
                        "id" => payload["id"] = json!(format!("{ID_PREFIX}{}", "0".repeat(64))),
                        "source" => {
                            payload["source_id"] = json!(OTHER_SOURCE);
                            tx.execute(
                                "UPDATE install_history SET source_id=?1 WHERE id=?2",
                                params![OTHER_SOURCE, id],
                            )?;
                        }
                        "binding" => payload["instance_id"] = json!(InstanceId::new()),
                        "sequence" => payload["source"]["sequence"] = json!(0),
                        "legacy" => {
                            payload["source"]["targets"][1]["id"] = json!("eeeeeeeeeeeeeeee")
                        }
                        _ => unreachable!(),
                    }
                    tx.execute(
                        "UPDATE install_history SET payload=?1 WHERE id=?2",
                        params![serde_json::to_vec(&payload).unwrap(), id],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(
                    store.transaction(|tx| batch.verify_in(tx)),
                    Err(HistoryError::Conflict)
                ),
                "{corruption}"
            );
            if !matches!(corruption, "source" | "missing") {
                assert!(
                    matches!(
                        read(&store, &legacy, &instance, None),
                        Err(HistoryError::Conflict)
                    ),
                    "{corruption}"
                );
            }
            if corruption != "missing" {
                assert!(
                    matches!(
                        store.transaction(|tx| batch.insert_in(tx)),
                        Err(HistoryError::Conflict)
                    ),
                    "{corruption}"
                );
            } else {
                assert_eq!(
                    count(&store),
                    2,
                    "verification must not repair missing evidence"
                );
            }
        }
    }

    #[test]
    fn successful_history_same_identity_different_evidence_conflicts_and_source_identity_separates()
    {
        let (store, instance, legacy, records) = fixture();
        let batch = PreparedImport::bind(records.clone(), &legacy, &instance).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        let mut changed = source_records();
        changed[0].sequence = 99;
        let conflict = PreparedImport::bind(prepare(changed), &legacy, &instance).unwrap();
        assert!(matches!(
            store.transaction(|tx| conflict.insert_in(tx)),
            Err(HistoryError::Conflict)
        ));
        let other = source_records()
            .into_iter()
            .map(|source| PreparedOperation::prepare(OTHER_SOURCE, source).unwrap())
            .collect();
        let other = PreparedImport::bind(other, &legacy, &instance).unwrap();
        store.transaction(|tx| other.insert_in(tx)).unwrap();
        assert_eq!(count(&store), 6);
        assert_eq!(
            read(&store, &legacy, &instance, None)
                .unwrap()
                .records
                .len(),
            3
        );
        assert!(PreparedImport::bind(vec![records[0].clone(); 129], &legacy, &instance).is_err());
        assert!(PreparedImport::bind(vec![records[0].clone(); 2], &legacy, &instance).is_err());
    }

    #[test]
    fn successful_history_pages_remain_bounded_and_survive_reopen_without_live_state() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("history.sqlite");
        let instance = InstanceId::new();
        let legacy = "aaaaaaaaaaaaaaaa";
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let original = source_records().remove(0);
        let records = (1..=35)
            .map(|number| {
                let mut source = original.clone();
                source.operation_id = format!("op-00000000-0000-4000-8000-{number:012x}");
                source.journal_id = format!("journal-{}", source.operation_id);
                source.sequence = number;
                source.targets[0].id = format!("install-{number:032x}");
                PreparedOperation::prepare(SOURCE, source).unwrap()
            })
            .collect();
        let batch = PreparedImport::bind(records, legacy, &instance).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        let first = read(&store, legacy, &instance, None).unwrap();
        assert_eq!(first.records.len(), PAGE_SIZE);
        assert_eq!(
            first.next_after.as_deref(),
            first.records.last().map(|record| record.id.as_str())
        );
        drop(store);
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        store.transaction(|tx| batch.verify_in(tx)).unwrap();
        let second = read(&store, legacy, &instance, first.next_after.as_deref()).unwrap();
        assert_eq!(second.records.len(), 3);
        assert!(second.next_after.is_none());
        let ids: BTreeSet<_> = first
            .records
            .iter()
            .chain(&second.records)
            .map(|record| &record.id)
            .collect();
        assert_eq!(ids.len(), 35);
        assert!(read(&store, legacy, &instance, Some("../unknown")).is_err());
        assert!(
            store
                .read(|db| read_in(db, OTHER_SOURCE, legacy, &instance, None))
                .unwrap()
                .records
                .is_empty()
        );
        let live_tables: i64 = store.read(|db| db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('install_queue','installed_versions','content_batches')",
            [], |row| row.get(0),
        ).map_err(StorageError::from)).unwrap();
        assert_eq!(live_tables, 0);
    }

    #[test]
    fn initialization_cancelled_history_preserves_failed_reason_without_execution_state() {
        let (store, instance, legacy, mut records) = fixture();
        let source = cancelled_initialization();
        records.push(PreparedOperation::prepare(SOURCE, source.clone()).unwrap());
        let batch = PreparedImport::bind(records, &legacy, &instance).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.transaction(|tx| batch.verify_in(tx)).unwrap();
        let page = read(&store, &legacy, &instance, None).unwrap();
        let wire = serde_json::to_value(&page).unwrap();
        let records = wire["records"].as_array().unwrap();
        assert_eq!(records.len(), 4);
        let saved = records
            .iter()
            .find(|record| record["operation_id"] == source.operation_id)
            .unwrap();
        assert_eq!(saved["instance_id"], instance.as_str());
        assert_eq!(saved["historical"], true);
        assert_eq!(saved["command"], "ModifyInstanceContent");
        assert_eq!(saved["outcome"], "Failed");
        assert_eq!(saved["failure_point"], "content_initialization_cancelled");
        assert_eq!(saved["sequence"], source.sequence.to_string());
        assert_eq!(
            saved["targets"],
            serde_json::to_value(&source.targets).unwrap()
        );
        assert_eq!(
            saved["completed_steps"],
            json!([{
                "step_id":"content_progress_initializing", "phase":"Failed", "result":"Failed",
                "changed_target":null, "generated_facts":["install_phase:initializing", "install_done:true", "install_error:true"],
                "rollback":"NotApplicable", "metrics":null
            }])
        );
        assert!(saved.get("allowed_actions").is_none());
        assert!(saved.get("status").is_none());
        assert!(saved.get("created_at").is_none());
        for success in records
            .iter()
            .filter(|record| record["outcome"] == "Succeeded")
        {
            assert!(success.get("failure_point").is_none());
        }
        assert_eq!(count(&store), 4);
    }

    #[test]
    fn initialization_cancelled_history_does_not_admit_worker_failures_or_effects() {
        let source = cancelled_initialization();
        PreparedOperation::prepare(SOURCE, source.clone()).unwrap();
        let mut invalid = Vec::new();
        for mutate in [
            |record: &mut SourceOperation| record.status = "Cancelled".into(),
            |record: &mut SourceOperation| record.outcome = Some("Cancelled".into()),
            |record: &mut SourceOperation| record.failure_point = None,
            |record: &mut SourceOperation| {
                record.failure_point = Some("content_worker_interrupted".into())
            },
            |record: &mut SourceOperation| {
                record.failure_point = Some("content_progress_initializing".into())
            },
            |record: &mut SourceOperation| {
                record.completed_steps[0].step_id = "content_progress_error".into()
            },
            |record: &mut SourceOperation| record.completed_steps[0].phase = "Downloading".into(),
            |record: &mut SourceOperation| record.completed_steps[0].result = "Completed".into(),
            |record: &mut SourceOperation| {
                record.completed_steps[0].generated_facts.pop();
            },
            |record: &mut SourceOperation| record.completed_steps[0].generated_facts.swap(1, 2),
            |record: &mut SourceOperation| record.completed_steps[0].rollback = "Applied".into(),
            |record: &mut SourceOperation| {
                record.completed_steps[0]
                    .guardian_fact_ids
                    .push("DownloadUnavailable".into())
            },
            |record: &mut SourceOperation| {
                record
                    .guardian_diagnosis_ids
                    .push("DownloadUnavailable".into())
            },
            |record: &mut SourceOperation| record.reconciliation_attempt = Some(json!({})),
            |record: &mut SourceOperation| {
                record.guardian_install_terminal =
                    rolled_back_records()[0].guardian_install_terminal.clone()
            },
        ] {
            let mut changed = source.clone();
            mutate(&mut changed);
            invalid.push(changed);
        }
        let successes = source_records();
        let mut with_metrics = source.clone();
        with_metrics.completed_steps[0].metrics =
            successes[2].completed_steps.last().unwrap().metrics.clone();
        invalid.push(with_metrics);
        let mut with_effect = source.clone();
        with_effect.completed_steps[0].changed_target = Some(source.targets[1].clone());
        invalid.push(with_effect);
        let mut with_progress = source.clone();
        with_progress
            .completed_steps
            .insert(0, successes[2].completed_steps[0].clone());
        invalid.push(with_progress);
        let mut vanilla = successes[0].clone();
        vanilla.status = source.status.clone();
        vanilla.outcome = source.outcome.clone();
        vanilla.failure_point = source.failure_point.clone();
        vanilla.completed_steps = source.completed_steps.clone();
        invalid.push(vanilla);
        for (index, record) in invalid.into_iter().enumerate() {
            assert!(
                matches!(
                    PreparedOperation::prepare(SOURCE, record),
                    Err(HistoryError::Invalid)
                ),
                "mutation {index}"
            );
        }
    }

    #[test]
    fn initialization_cancelled_history_readback_revalidates_reason_and_effect_absence() {
        for mutation in [
            "reason",
            "progress",
            "metrics",
            "changed_target",
            "guardian",
        ] {
            let (store, instance, legacy, _) = fixture();
            let operation = PreparedOperation::prepare(SOURCE, cancelled_initialization()).unwrap();
            let id = operation.0.id.clone();
            let batch = PreparedImport::bind(vec![operation], &legacy, &instance).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            let successes = source_records();
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT payload FROM install_history WHERE id=?1",
                        [&id],
                        |row| row.get(0),
                    )?;
                    let mut payload: Value = serde_json::from_slice(&bytes).unwrap();
                    match mutation {
                        "reason" => {
                            payload["source"]["failure_point"] = json!("content_worker_interrupted")
                        }
                        "progress" => payload["source"]["completed_steps"]
                            .as_array_mut()
                            .unwrap()
                            .insert(
                                0,
                                serde_json::to_value(&successes[2].completed_steps[0]).unwrap(),
                            ),
                        "metrics" => {
                            payload["source"]["completed_steps"][0]["metrics"] =
                                serde_json::to_value(
                                    &successes[2].completed_steps.last().unwrap().metrics,
                                )
                                .unwrap()
                        }
                        "changed_target" => {
                            payload["source"]["completed_steps"][0]["changed_target"] =
                                payload["source"]["targets"][1].clone()
                        }
                        "guardian" => payload["source"]["guardian_install_terminal"] = json!({}),
                        _ => unreachable!(),
                    }
                    tx.execute(
                        "UPDATE install_history SET payload=?1 WHERE id=?2",
                        params![serde_json::to_vec(&payload).unwrap(), id],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(
                    read(&store, &legacy, &instance, None),
                    Err(HistoryError::Conflict)
                ),
                "{mutation}"
            );
            assert!(
                matches!(
                    store.transaction(|tx| batch.verify_in(tx)),
                    Err(HistoryError::Conflict)
                ),
                "{mutation}"
            );
        }
    }

    #[test]
    fn rolled_back_history_preserves_neutral_proof_and_excludes_guardian_annotations() {
        for diagnostics in [true, false] {
            let (store, instance, legacy, _) = fixture();
            let mut sources = rolled_back_records();
            if !diagnostics {
                for source in &mut sources {
                    source.guardian_diagnosis_ids.clear();
                    source.guardian_install_terminal = None;
                    source
                        .completed_steps
                        .last_mut()
                        .unwrap()
                        .guardian_fact_ids
                        .clear();
                }
            }
            sources[2].sequence = u64::MAX - 1;
            let batch = PreparedImport::bind(prepare(sources.clone()), &legacy, &instance).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            store.transaction(|tx| batch.verify_in(tx)).unwrap();
            let wire =
                serde_json::to_value(read(&store, &legacy, &instance, None).unwrap()).unwrap();
            assert_eq!(wire["records"].as_array().unwrap().len(), 3);
            for source in sources {
                let saved = wire["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| record["operation_id"] == source.operation_id)
                    .unwrap();
                assert_eq!(saved["outcome"], "Failed");
                assert_eq!(saved["failure_point"], "install_progress_error");
                assert_eq!(saved["sequence"], source.sequence.to_string());
                assert_eq!(saved["rollback"], "NotApplicable");
                assert!(saved["instance_id"].is_null());
                assert!(saved.get("guardian_install_terminal").is_none());
                assert!(saved.get("guardian_diagnosis_ids").is_none());
                assert!(saved.get("allowed_actions").is_none());
                let mut neutral = serde_json::to_value(&source.completed_steps).unwrap();
                for step in neutral.as_array_mut().unwrap() {
                    step.as_object_mut().unwrap().remove("guardian_fact_ids");
                }
                assert_eq!(saved["completed_steps"], neutral);
            }
            assert_eq!(count(&store), 3);
        }
    }

    #[test]
    fn rolled_back_history_requires_exact_settled_checkpoint_sequence() {
        let sources = rolled_back_records();
        let mut invalid = Vec::new();
        for mutate in [
            |source: &mut SourceOperation| {
                source.failure_point = Some("install_worker_interrupted".into())
            },
            |source: &mut SourceOperation| source.rollback = "Applied".into(),
            |source: &mut SourceOperation| {
                source.completed_steps.remove(0);
            },
            |source: &mut SourceOperation| {
                source.completed_steps.remove(1);
            },
            |source: &mut SourceOperation| {
                source.completed_steps.pop();
            },
            |source: &mut SourceOperation| source.completed_steps[1].phase = "Installing".into(),
            |source: &mut SourceOperation| {
                source.completed_steps[1].rollback = "NotApplicable".into()
            },
            |source: &mut SourceOperation| {
                source.completed_steps[1]
                    .changed_target
                    .as_mut()
                    .unwrap()
                    .id = "another".into()
            },
            |source: &mut SourceOperation| {
                source.completed_steps[1].generated_facts[1] =
                    "install_publication_version_id:another".into()
            },
            |source: &mut SourceOperation| {
                source.completed_steps[1].generated_facts[2] =
                    "install_publication_evidence:invalid".into()
            },
            |source: &mut SourceOperation| {
                source.completed_steps.last_mut().unwrap().rollback = "Applied".into()
            },
            |source: &mut SourceOperation| {
                source
                    .completed_steps
                    .last_mut()
                    .unwrap()
                    .generated_facts
                    .swap(1, 2)
            },
            |source: &mut SourceOperation| {
                source.completed_steps.last_mut().unwrap().changed_target =
                    Some(source.targets[1].clone())
            },
            |source: &mut SourceOperation| {
                source.completed_steps[0]
                    .guardian_fact_ids
                    .push("download_interrupted".into())
            },
            |source: &mut SourceOperation| source.reconciliation_terminal = Some(json!({})),
        ] {
            let mut changed = sources[0].clone();
            mutate(&mut changed);
            invalid.push(changed);
        }
        let successes = source_records();
        let mut activation_on_rollback = sources[0].clone();
        activation_on_rollback.completed_steps[1]
            .generated_facts
            .push(successes[0].completed_steps[1].generated_facts[3].clone());
        invalid.push(activation_on_rollback);
        let mut metrics = sources[0].clone();
        metrics.completed_steps.last_mut().unwrap().metrics =
            successes[2].completed_steps.last().unwrap().metrics.clone();
        invalid.push(metrics);
        let mut after_rollback = sources[0].clone();
        let mut progress = successes[2].completed_steps[0].clone();
        progress.step_id = "install_progress_download".into();
        after_rollback.completed_steps.insert(2, progress);
        invalid.push(after_rollback);
        let mut missing_base = sources[2].clone();
        missing_base.completed_steps.remove(1);
        invalid.push(missing_base);
        let mut repeated_base = sources[2].clone();
        repeated_base.completed_steps[2] = sources[1].completed_steps[1].clone();
        invalid.push(repeated_base);
        let mut committed_after_rollback = sources[1].clone();
        committed_after_rollback
            .completed_steps
            .insert(2, successes[1].completed_steps[2].clone());
        invalid.push(committed_after_rollback);
        for (index, source) in invalid.into_iter().enumerate() {
            assert!(
                matches!(
                    PreparedOperation::prepare(SOURCE, source),
                    Err(HistoryError::Invalid)
                ),
                "mutation {index}"
            );
        }
    }

    #[test]
    fn rolled_back_history_validates_excluded_raw_schema_labels_and_historical_window() {
        let original = rolled_back_records().remove(1);
        let raw = serde_json::to_string(&original).unwrap();
        for (field, replacement) in [
            (
                "\"action\":\"Retry\"",
                "\"action\":\"Retry\",\"action\":\"Retry\"",
            ),
            ("\"diagnosis_id\":", "\"unknown\":true,\"diagnosis_id\":"),
            ("\"binding\":", "\"unknown\":true,\"binding\":"),
            (
                "\"observed_at\":\"2026-08-13T10:00:00.000Z\"",
                "\"observed_at\":\"2026-08-13T10:00:00.000Z\",\"observed_at\":\"2026-08-13T10:00:00.000Z\"",
            ),
            (
                "\"system\":\"Execution\"",
                "\"system\":\"Execution\",\"system\":\"Execution\"",
            ),
        ] {
            let changed = raw.replacen(field, replacement, 1);
            assert_ne!(changed, raw);
            assert!(
                serde_json::from_str::<SourceOperation>(&changed).is_err(),
                "{field}"
            );
        }
        for year in ["2000", "2099"] {
            let mut historical = original.clone();
            let memory = historical
                .guardian_install_terminal
                .as_mut()
                .unwrap()
                .memory
                .as_mut()
                .unwrap();
            memory.target.ownership = "ExternalProviderDerived".into();
            memory.observed_at = format!("{year}-01-01T00:00:00.000Z");
            memory.suppression_until = format!("{year}-01-01T00:05:00.000Z");
            PreparedOperation::prepare(SOURCE, historical).unwrap();
        }
        let mut exact_bounds = original.clone();
        exact_bounds.guardian_diagnosis_ids = GUARDIAN_DIAGNOSIS_IDS[..32]
            .iter()
            .map(|id| (*id).into())
            .collect();
        exact_bounds
            .completed_steps
            .last_mut()
            .unwrap()
            .guardian_fact_ids = GUARDIAN_FACT_IDS[..64]
            .iter()
            .map(|id| (*id).into())
            .collect();
        PreparedOperation::prepare(SOURCE, exact_bounds).unwrap();
        for mutate in [
            |source: &mut SourceOperation| {
                source
                    .guardian_diagnosis_ids
                    .push("unknown_diagnosis".into())
            },
            |source: &mut SourceOperation| {
                source
                    .guardian_diagnosis_ids
                    .push("download_unavailable".into())
            },
            |source: &mut SourceOperation| {
                source.guardian_diagnosis_ids = GUARDIAN_DIAGNOSIS_IDS[..33]
                    .iter()
                    .map(|id| (*id).into())
                    .collect()
            },
            |source: &mut SourceOperation| source.guardian_diagnosis_ids.clear(),
            |source: &mut SourceOperation| {
                source
                    .completed_steps
                    .last_mut()
                    .unwrap()
                    .guardian_fact_ids
                    .push("unknown_fact".into())
            },
            |source: &mut SourceOperation| {
                source
                    .completed_steps
                    .last_mut()
                    .unwrap()
                    .guardian_fact_ids
                    .push("download_provider_unavailable".into())
            },
            |source: &mut SourceOperation| {
                source.completed_steps.last_mut().unwrap().guardian_fact_ids = GUARDIAN_FACT_IDS
                    [..65]
                    .iter()
                    .map(|id| (*id).into())
                    .collect()
            },
            |source: &mut SourceOperation| {
                source
                    .guardian_install_terminal
                    .as_mut()
                    .unwrap()
                    .diagnosis_id = "install_execution_failed".into()
            },
            |source: &mut SourceOperation| {
                source.guardian_install_terminal.as_mut().unwrap().action = "Unknown".into()
            },
            |source: &mut SourceOperation| {
                source.guardian_install_terminal.as_mut().unwrap().action = "Block".into()
            },
            |source: &mut SourceOperation| {
                source.guardian_install_terminal.as_mut().unwrap().memory = None
            },
        ] {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert!(matches!(
                PreparedOperation::prepare(SOURCE, changed),
                Err(HistoryError::Invalid)
            ));
        }
        for mutate in [
            |memory: &mut SourceGuardianMemory| memory.binding.make_ascii_uppercase(),
            |memory: &mut SourceGuardianMemory| memory.target.system = "Application".into(),
            |memory: &mut SourceGuardianMemory| memory.target.kind = "Version".into(),
            |memory: &mut SourceGuardianMemory| memory.target.ownership = "UserOwned".into(),
            |memory: &mut SourceGuardianMemory| memory.target.id = "bearer".into(),
            |memory: &mut SourceGuardianMemory| memory.target.id = "clean+artifact".into(),
            |memory: &mut SourceGuardianMemory| memory.observed_at = "2026-08-13T10:00:00Z".into(),
            |memory: &mut SourceGuardianMemory| {
                memory.observed_at = "2026-08-13T10:00:00.000+00:00".into()
            },
            |memory: &mut SourceGuardianMemory| {
                memory.suppression_until = "2026-08-13T10:06:00.000Z".into()
            },
        ] {
            let mut changed = original.clone();
            mutate(
                changed
                    .guardian_install_terminal
                    .as_mut()
                    .unwrap()
                    .memory
                    .as_mut()
                    .unwrap(),
            );
            assert!(matches!(
                PreparedOperation::prepare(SOURCE, changed),
                Err(HistoryError::Invalid)
            ));
        }
    }

    #[test]
    fn rolled_back_history_readback_revalidates_checkpoint_and_excluded_terminal() {
        for mutation in [
            "rollback",
            "evidence",
            "diagnosis",
            "action",
            "window",
            "target",
        ] {
            let (store, instance, legacy, _) = fixture();
            let operation =
                PreparedOperation::prepare(SOURCE, rolled_back_records().remove(1)).unwrap();
            let id = operation.0.id.clone();
            let batch = PreparedImport::bind(vec![operation], &legacy, &instance).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT payload FROM install_history WHERE id=?1",
                        [&id],
                        |row| row.get(0),
                    )?;
                    let mut payload: Value = serde_json::from_slice(&bytes).unwrap();
                    let source = &mut payload["source"];
                    match mutation {
                        "rollback" => {
                            source["completed_steps"][1]["rollback"] = json!("NotApplicable")
                        }
                        "evidence" => {
                            source["completed_steps"][1]["generated_facts"][2] =
                                json!("install_publication_evidence:invalid")
                        }
                        "diagnosis" => source["guardian_diagnosis_ids"] = json!([]),
                        "action" => source["guardian_install_terminal"]["action"] = json!("Block"),
                        "window" => {
                            source["guardian_install_terminal"]["memory"]["suppression_until"] =
                                json!("2026-08-13T10:06:00.000Z")
                        }
                        "target" => {
                            source["guardian_install_terminal"]["memory"]["target"]["id"] =
                                json!("bearer")
                        }
                        _ => unreachable!(),
                    }
                    tx.execute(
                        "UPDATE install_history SET payload=?1 WHERE id=?2",
                        params![serde_json::to_vec(&payload).unwrap(), id],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(
                    read(&store, &legacy, &instance, None),
                    Err(HistoryError::Conflict)
                ),
                "{mutation}"
            );
            assert!(
                matches!(
                    store.transaction(|tx| batch.verify_in(tx)),
                    Err(HistoryError::Conflict)
                ),
                "{mutation}"
            );
        }
    }

    #[test]
    fn global_history_receipt_pages_preserve_the_snapshot_across_appends_and_reopen() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("history.sqlite");
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let batch = PreparedImport::bind_global(global_records(35)).unwrap();
        let proof = batch.completion_proof(SOURCE).unwrap();
        assert_eq!(proof.count(), 35);
        let encoded = serde_json::to_vec(&proof).unwrap();
        assert!(encoded.len() <= MAX_COMPLETION_PROOF_BYTES);
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        let first = store
            .read(|db| read_completed_in(db, SOURCE, Some(&proof), None, None))
            .unwrap();
        assert_eq!(first.records.len(), PAGE_SIZE);
        assert!(
            first
                .records
                .iter()
                .all(|record| record.instance_id.is_none())
        );

        let content = prepare(source_records()).remove(2);
        let legacy = content.legacy_instance_id().unwrap().to_owned();
        let later = PreparedImport::bind(
            vec![global_records(36).pop().unwrap(), content],
            &legacy,
            &InstanceId::new(),
        )
        .unwrap();
        store.transaction(|tx| later.insert_in(tx)).unwrap();
        let other = PreparedImport::bind_global(vec![
            PreparedOperation::prepare(OTHER_SOURCE, source_records().remove(0)).unwrap(),
        ])
        .unwrap();
        store.transaction(|tx| other.insert_in(tx)).unwrap();
        assert_eq!(count(&store), 38);
        drop(store);

        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[MIGRATION]).unwrap();
        let proof: CompletionProof = serde_json::from_slice(&encoded).unwrap();
        store.read(|db| proof.verify_in(db, SOURCE)).unwrap();
        assert_eq!(
            first,
            store
                .read(|db| read_completed_in(db, SOURCE, Some(&proof), None, None))
                .unwrap()
        );
        let last = store
            .read(|db| {
                read_completed_in(db, SOURCE, Some(&proof), None, first.next_after.as_deref())
            })
            .unwrap();
        assert_eq!(last.records.len(), 3);
        assert!(last.next_after.is_none());
        let ids: Vec<_> = first
            .records
            .iter()
            .chain(&last.records)
            .map(|row| row.id.clone())
            .collect();
        assert_eq!(ids, proof.ids);
        assert!(
            store
                .read(|db| read_completed_in(db, SOURCE, Some(&proof), None, Some("../invalid")))
                .is_err()
        );
        assert!(
            store
                .read(|db| read_completed_in(db, OTHER_SOURCE, Some(&proof), None, None))
                .is_err()
        );
    }

    #[test]
    fn global_history_binding_and_completion_proof_are_strict_and_empty_is_source_bound() {
        let (store, instance, legacy, records) = fixture();
        assert!(PreparedImport::bind_global(records.clone()).is_err());
        assert!(
            PreparedImport::bind(records, &legacy, &instance)
                .unwrap()
                .completion_proof(SOURCE)
                .is_err()
        );
        assert!(PreparedImport::bind_global(global_records(129)).is_err());
        let records = global_records(2);
        assert!(PreparedImport::bind_global(vec![records[0].clone(); 2]).is_err());
        let mut other = source_records().remove(0);
        other.sequence = 2;
        assert!(
            PreparedImport::bind_global(vec![
                records[0].clone(),
                PreparedOperation::prepare(OTHER_SOURCE, other).unwrap()
            ])
            .is_err()
        );

        let batch = PreparedImport::bind_global(records).unwrap();
        let proof = batch.completion_proof(SOURCE).unwrap();
        assert!(batch.completion_proof(OTHER_SOURCE).is_err());
        store.transaction(|tx| batch.insert_in(tx)).unwrap();
        let empty = PreparedImport::bind_global(vec![]).unwrap();
        let empty_proof = empty.completion_proof(SOURCE).unwrap();
        assert_eq!(empty_proof.count(), 0);
        assert_ne!(
            empty_proof.digest,
            empty.completion_proof(OTHER_SOURCE).unwrap().digest
        );
        let page = store
            .read(|db| read_completed_in(db, SOURCE, Some(&empty_proof), None, None))
            .unwrap();
        assert!(page.records.is_empty());
        assert!(page.next_after.is_none());
        assert_eq!(count(&store), 2);

        for mutation in ["order", "duplicate", "id", "digest", "source", "count"] {
            let mut changed = proof.clone();
            match mutation {
                "order" => changed.ids.reverse(),
                "duplicate" => changed.ids[1] = changed.ids[0].clone(),
                "id" => changed.ids[0] = "unknown".into(),
                "digest" => changed.digest = "0".repeat(64),
                "source" => changed.source_id = OTHER_SOURCE.into(),
                "count" => {
                    changed.ids = global_records(129)
                        .iter()
                        .map(|record| record.0.id.clone())
                        .collect();
                    changed.ids.sort();
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    store.read(|db| changed.verify_in(db, SOURCE)),
                    Err(HistoryError::Conflict)
                ),
                "{mutation}"
            );
        }
        let encoded = serde_json::to_string(&proof).unwrap();
        let duplicate = format!("{{\"source_id\":\"{SOURCE}\",{}", &encoded[1..]);
        assert!(serde_json::from_str::<CompletionProof>(&duplicate).is_err());
        let mut unknown = serde_json::to_value(&proof).unwrap();
        unknown["unknown"] = json!(true);
        assert!(serde_json::from_value::<CompletionProof>(unknown).is_err());
    }

    #[test]
    fn global_history_completion_checks_records_beyond_the_page_without_repair() {
        for corruption in ["missing", "valid_change", "source", "binding", "step"] {
            let (store, _, _, _) = fixture();
            let batch = PreparedImport::bind_global(global_records(35)).unwrap();
            let proof = batch.completion_proof(SOURCE).unwrap();
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            let id = proof.ids.last().unwrap();
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    if corruption == "missing" {
                        tx.execute("DELETE FROM install_history WHERE id=?1", [id])?;
                        return Ok(());
                    }
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT payload FROM install_history WHERE id=?1",
                        [id],
                        |row| row.get(0),
                    )?;
                    let mut payload: Value = serde_json::from_slice(&bytes).unwrap();
                    match corruption {
                        "valid_change" => payload["source"]["sequence"] = json!(99),
                        "source" => {
                            tx.execute(
                                "UPDATE install_history SET source_id=?1 WHERE id=?2",
                                params![OTHER_SOURCE, id],
                            )?;
                        }
                        "binding" => {
                            tx.execute(
                                "UPDATE install_history SET instance_id=?1 WHERE id=?2",
                                params![InstanceId::new().as_str(), id],
                            )?;
                        }
                        "step" => {
                            payload["source"]["completed_steps"][0]["generated_facts"] =
                                json!(["future:effect"])
                        }
                        _ => unreachable!(),
                    }
                    tx.execute(
                        "UPDATE install_history SET payload=?1 WHERE id=?2",
                        params![serde_json::to_vec(&payload).unwrap(), id],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(
                    store.read(|db| proof.verify_in(db, SOURCE)),
                    Err(HistoryError::Conflict)
                ),
                "{corruption}"
            );
            assert!(
                matches!(
                    store.read(|db| read_completed_in(db, SOURCE, Some(&proof), None, None)),
                    Err(HistoryError::Conflict)
                ),
                "{corruption}"
            );
            assert_eq!(count(&store), if corruption == "missing" { 34 } else { 35 });
        }
    }

    #[test]
    fn global_history_publication_rolls_back_ignored_or_corrupted_writes() {
        for rewrite in [false, true] {
            let (store, _, _, _) = fixture();
            let batch = PreparedImport::bind_global(global_records(2)).unwrap();
            let proof = batch.completion_proof(SOURCE).unwrap();
            let first = &batch.0[0].operation.0.id;
            let second = &batch.0[1].operation.0.id;
            let trigger = if rewrite {
                format!(
                    "CREATE TRIGGER break_history AFTER INSERT ON install_history WHEN NEW.id='{second}' BEGIN UPDATE install_history SET payload=CAST('{{}}' AS BLOB) WHERE id='{first}'; END;"
                )
            } else {
                format!(
                    "CREATE TRIGGER break_history BEFORE INSERT ON install_history WHEN NEW.id='{second}' BEGIN SELECT RAISE(IGNORE); END;"
                )
            };
            store
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(&trigger)?;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(
                store.transaction(|tx| {
                    batch.insert_in(tx)?;
                    proof.verify_in(tx, SOURCE)
                }),
                Err(HistoryError::Conflict)
            ));
            assert_eq!(count(&store), 0);
            assert!(store.read(|db| proof.verify_in(db, SOURCE)).is_err());
            assert_eq!(count(&store), 0);
        }
    }
}
