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
use std::{collections::BTreeSet, sync::Arc};
use ts_rs::TS;

const MAX_RECORD_BYTES: usize = 256 * 1024;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_RECORDS: usize = 128;
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
    pub(crate) guardian_install_terminal: Option<serde_json::Value>,
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
        let mut stored_bytes = serde_json::to_vec(&StoredRef {
            schema: 1,
            id: &id,
            source_id,
            instance_id: None,
            source: &source,
        })
        .map_err(|_| HistoryError::Invalid)?
        .len();
        // A canonical UUID string occupies 38 JSON bytes instead of null's 4.
        if legacy_instance_id.is_some() {
            stored_bytes += 34;
        }
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

pub(crate) struct PreparedImport(Vec<BoundOperation>);

struct BoundOperation {
    operation: PreparedOperation,
    instance_id: Option<InstanceId>,
    bytes: Vec<u8>,
}

#[derive(Serialize)]
struct StoredRef<'a> {
    schema: u8,
    id: &'a str,
    source_id: &'a str,
    instance_id: Option<&'a InstanceId>,
    source: &'a SourceOperation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    schema: u8,
    id: String,
    source_id: String,
    instance_id: Option<InstanceId>,
    source: SourceOperation,
}

struct ReadRecord {
    operation: PreparedOperation,
    instance_id: Option<InstanceId>,
}

impl PreparedImport {
    pub(crate) fn validate(records: &[PreparedOperation]) -> Result<(), HistoryError> {
        if records.len() > MAX_BATCH_RECORDS {
            return Err(HistoryError::Invalid);
        }
        let mut ids = BTreeSet::new();
        let mut sequences = BTreeSet::new();
        let source_id = records.first().map(|record| &record.0.source_id);
        let mut total = 0usize;
        for record in records {
            let value = &record.0;
            total = total
                .checked_add(value.stored_bytes)
                .ok_or(HistoryError::Invalid)?;
            if !ids.insert(&value.id)
                || !sequences.insert(value.source.sequence)
                || Some(&value.source_id) != source_id
                || total > MAX_BATCH_BYTES
            {
                return Err(HistoryError::Invalid);
            }
        }
        Ok(())
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
            let value = &operation.0;
            let instance_id = match operation.legacy_instance_id() {
                None => None,
                Some(id) if id == legacy_id => Some(instance.clone()),
                Some(_) => return Err(HistoryError::Invalid),
            };
            let bytes = serde_json::to_vec(&StoredRef {
                schema: 1,
                id: &value.id,
                source_id: &value.source_id,
                instance_id: instance_id.as_ref(),
                source: &value.source,
            })
            .map_err(|_| HistoryError::Invalid)?;
            if bytes.len() != value.stored_bytes || bytes.len() > MAX_RECORD_BYTES {
                return Err(HistoryError::Invalid);
            }
            bound.push(BoundOperation {
                operation,
                instance_id,
                bytes,
            });
        }
        Ok(Self(bound))
    }

    pub(crate) fn insert_in(&self, tx: &Transaction<'_>) -> Result<(), HistoryError> {
        for record in &self.0 {
            let value = &record.operation.0;
            match stored(tx, &value.id)? {
                Some(saved) if record.matches(&saved) => continue,
                Some(_) => return Err(HistoryError::Conflict),
                None => {}
            }
            if tx.execute(
                "INSERT INTO install_history(id,source_id,instance_id,payload) VALUES(?1,?2,?3,?4)",
                params![
                    value.id,
                    value.source_id,
                    record.instance_id.as_ref().map(InstanceId::as_str),
                    record.bytes
                ],
            )? != 1
            {
                return Err(HistoryError::Conflict);
            }
        }
        // A later INSERT trigger can affect an earlier row in this same batch.
        self.verify_in(tx)
    }

    pub(crate) fn verify_in(&self, tx: &Transaction<'_>) -> Result<(), HistoryError> {
        for record in &self.0 {
            if stored(tx, &record.operation.0.id)?
                .as_ref()
                .is_none_or(|saved| !record.matches(saved))
            {
                return Err(HistoryError::Conflict);
            }
        }
        Ok(())
    }
}

impl BoundOperation {
    fn matches(&self, saved: &ReadRecord) -> bool {
        self.operation.0.id == saved.operation.0.id
            && self.operation.0.source_id == saved.operation.0.source_id
            && self.operation.0.source == saved.operation.0.source
            && self.instance_id == saved.instance_id
    }
}

fn stored(db: &Connection, id: &str) -> Result<Option<ReadRecord>, HistoryError> {
    let row: Option<(String, Option<String>, Option<Vec<u8>>)> = db
        .query_row(
            "SELECT source_id,instance_id,CASE WHEN length(payload)<=262144 THEN payload END
         FROM install_history WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(source, instance, bytes)| decode_stored(id, &source, instance.as_deref(), bytes))
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
        || saved.instance_id.as_ref().map(InstanceId::as_str) != instance_id
    {
        return Err(HistoryError::Conflict);
    }
    let operation = PreparedOperation::prepare(&saved.source_id, saved.source)
        .map_err(|_| HistoryError::Conflict)?;
    if operation.0.id != id
        || operation.legacy_instance_id().is_some() != saved.instance_id.is_some()
    {
        return Err(HistoryError::Conflict);
    }
    Ok(ReadRecord {
        operation,
        instance_id: saved.instance_id,
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
         SELECT h.id,h.source_id,h.instance_id,
           CASE WHEN length(h.payload)<=262144 THEN h.payload END
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
            || saved.instance_id.as_ref().is_some_and(|id| id != instance)
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
            instance_id: self.instance_id.as_ref().map(|id| id.as_str().to_owned()),
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
            outcome: source
                .outcome
                .clone()
                .expect("validated successful history"),
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
        || source.status != "Succeeded"
        || source.outcome.as_deref() != Some("Succeeded")
        || source.failure_point.is_some()
        || source.rollback != "NotApplicable"
        || !source.guardian_diagnosis_ids.is_empty()
        || source.reconciliation_attempt.is_some()
        || source.reconciliation_terminal.is_some()
        || source.persisted_state_repair_attempt.is_some()
        || source.persisted_state_repair_terminal.is_some()
        || source.guardian_install_terminal.is_some()
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
    let namespace = if matches!(&identity, Identity::Content(_)) {
        "content"
    } else {
        "install"
    };
    let mut seen = BTreeSet::new();
    let mut checkpoints = 0usize;
    let mut recovering_seen = false;
    for (index, step) in source.completed_steps.iter().enumerate() {
        if !seen.insert(&step.step_id)
            || !structured_token(&step.step_id, 96)
            || !step.guardian_fact_ids.is_empty()
            || step.rollback != "NotApplicable"
        {
            return Err(HistoryError::Invalid);
        }
        if index + 1 == source.completed_steps.len() {
            validate_progress(step, namespace, true)?;
            continue;
        }
        if step.step_id.starts_with(&format!("{namespace}_progress_")) {
            validate_progress(step, namespace, false)?;
            recovering_seen |= step.step_id == "install_progress_recovering";
            continue;
        }
        let (kind, version) = match (&identity, checkpoints) {
            (Identity::Vanilla(version), 0) => ("committed", version),
            (Identity::Loader { base, .. }, 0) => ("base_committed", base),
            (Identity::Loader { version, .. }, 1) => ("child_committed", version),
            _ => return Err(HistoryError::Invalid),
        };
        if !recovering_seen {
            return Err(HistoryError::Invalid);
        }
        validate_checkpoint(step, kind, version)?;
        checkpoints += 1;
    }
    match identity {
        Identity::Vanilla(_) if checkpoints == 1 => Ok(None),
        Identity::Loader { .. } if checkpoints == 2 => Ok(None),
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

fn validate_checkpoint(step: &SourceStep, kind: &str, version: &str) -> Result<(), HistoryError> {
    let expected_step = match kind {
        "committed" => "install_publication_committed",
        "base_committed" => "install_base_publication_committed",
        "child_committed" => "install_child_publication_committed",
        _ => return Err(HistoryError::Invalid),
    };
    let [publication, recorded_version, evidence, contract] = step.generated_facts.as_slice()
    else {
        return Err(HistoryError::Invalid);
    };
    let evidence = evidence
        .strip_prefix("install_publication_evidence:")
        .and_then(|id| ManagedInstallPublicationEvidenceId::parse(id).ok())
        .ok_or(HistoryError::Invalid)?;
    if step.step_id != expected_step
        || step.phase != "Installing"
        || step.result != "Completed"
        || step.metrics.is_some()
        || publication != &format!("install_publication:{kind}")
        || recorded_version != &format!("install_publication_version_id:{version}")
        || !evidence.matches_version_id(version)
        || contract
            .strip_prefix("install_activation_contract:")
            .and_then(|id| ManagedInstallActivationContractId::parse(id).ok())
            .is_none()
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

    fn prepare(records: Vec<SourceOperation>) -> Vec<PreparedOperation> {
        records
            .into_iter()
            .map(|record| PreparedOperation::prepare(SOURCE, record).unwrap())
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
            |record: &mut SourceOperation| record.guardian_install_terminal = Some(json!({})),
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
}
