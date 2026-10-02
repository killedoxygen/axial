//! Instance-only predecessor publication. The durable source mapping and the
//! ordinary creation reservation commit together; the source grants read access
//! only, and every destination byte is independently staged.

use super::{
    copy::{CopyBudget, copy_tree},
    create::{InstanceService, PendingAdmission, PublicationSource, public_instance},
    model::{Instance, InstanceError, InstanceId, InstanceRecord, InstanceResult},
};
use crate::{
    files::{PortableName, ScopedDirectory},
    import::{ImportError, PreparedInstanceImport},
    library::GenerationPin,
    storage::{
        Migration,
        rusqlite::{OptionalExtension, Row, Transaction, params},
    },
    tasks::{CancellationToken, ExclusionLease, TaskHandle},
};
use std::collections::BTreeMap;

pub const MIGRATION: Migration = Migration {
    id: "instance_imports.v1",
    sql: "CREATE TABLE instance_imports (
        source_id TEXT NOT NULL,
        legacy_id TEXT NOT NULL,
        fingerprint TEXT NOT NULL,
        instance_id TEXT NOT NULL UNIQUE,
        PRIMARY KEY(source_id, legacy_id)
    );",
};

const MAPPING_FIELDS: &str = "m.legacy_id,m.instance_id,c.phase,
    CASE WHEN length(CAST(c.record_json AS BLOB))<=65536 THEN c.record_json END,
    CASE WHEN length(CAST(c.directory_receipt AS BLOB))<=4096 THEN c.directory_receipt END,
    i.lifecycle,
    CASE WHEN length(CAST(i.record_json AS BLOB))<=65536 THEN i.record_json END,
    CASE WHEN length(CAST(i.directory_receipt AS BLOB))<=4096 THEN i.directory_receipt END,
    i.library_id,i.directory_name,i.revision,i.name,
    CASE WHEN length(CAST(m.source_id AS BLOB))=64 THEN m.source_id END,
    CASE WHEN length(CAST(m.fingerprint AS BLOB))=64 THEN m.fingerprint END";

impl InstanceService {
    /// A read-only identity projection, not a payload or launch-readiness check.
    /// The completed creation and current live registration must agree on the
    /// exact destination identity; reservations and removed rows never map.
    pub(crate) fn completed_import_mappings(
        &self,
        source_id: &str,
        fingerprint: &str,
    ) -> InstanceResult<BTreeMap<String, String>> {
        for value in [source_id, fingerprint] {
            validate_mapping_digest(value)?;
        }
        self.registry().storage().read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {MAPPING_FIELDS}
                 FROM instance_imports m
                 LEFT JOIN instance_creations c ON c.instance_id=m.instance_id
                 LEFT JOIN instances i ON i.id=m.instance_id
                 WHERE m.source_id=?1 AND m.fingerprint=?2
                 ORDER BY m.legacy_id LIMIT 4097"
            ))?;
            let mut rows = statement.query(params![source_id, fingerprint])?;
            let mut result = BTreeMap::new();
            let mut count = 0;
            while let Some(row) = rows.next()? {
                count += 1;
                if count > 4096 {
                    return Err(InstanceError::InvalidInput);
                }
                let Some(mapping) = completed_mapping(row)? else {
                    continue;
                };
                if result
                    .insert(mapping.legacy_id, mapping.id.to_string())
                    .is_some()
                {
                    return Err(InstanceError::InvalidInput);
                }
            }
            Ok(result)
        })
    }

    /// Immutable imported evidence only. The stored completed mapping supplies
    /// source identity; a name, selected instance or version never substitutes.
    pub fn imported_install_history(
        &self,
        id: &InstanceId,
        after: Option<&str>,
    ) -> InstanceResult<crate::install::history::HistoryPage> {
        crate::install::history::validate_cursor(after).map_err(install_history_error)?;
        self.registry().storage().read(|connection| {
            let transaction = connection.unchecked_transaction()?;
            let mapping = {
                let mut statement = transaction.prepare(&format!(
                    "SELECT {MAPPING_FIELDS} FROM instances i
                     LEFT JOIN instance_imports m ON m.instance_id=i.id
                     LEFT JOIN instance_creations c ON c.instance_id=i.id
                     WHERE i.id=?1"
                ))?;
                let mut rows = statement.query([id.as_str()])?;
                let row = rows.next()?.ok_or(InstanceError::NotFound)?;
                current_mapping_record(row, id)?;
                if row.get::<_, Option<String>>(1)?.is_none() {
                    None
                } else {
                    Some(completed_mapping(row)?.ok_or(InstanceError::Conflict)?)
                }
            };
            let page = match mapping {
                Some(mapping) => crate::install::history::read_in(
                    &transaction,
                    &mapping.source_id,
                    &mapping.legacy_id,
                    &mapping.id,
                    after,
                )
                .map_err(install_history_error)?,
                None => crate::install::history::HistoryPage {
                    records: Vec::new(),
                    next_after: None,
                },
            };
            transaction.commit()?;
            Ok(page)
        })
    }

    /// Callers supply the immutable capability prepared by the import owner,
    /// never a source path, destination ID or caller-authored instance record.
    pub fn import_instance(
        &self,
        imported: PreparedInstanceImport,
    ) -> InstanceResult<TaskHandle<InstanceResult<Instance>>> {
        imported.revalidate().map_err(import_error)?;
        let import_key = format!("import:{}:{}", imported.source_id(), imported.legacy_id());
        let source_lease = self
            .directories
            .exclusions()
            .try_acquire([import_key.as_str()], [])
            .map_err(|_| InstanceError::Busy)?;
        let mut instance = imported.instance().clone();
        if let Some((id, fingerprint)) = self.import_mapping(&imported)? {
            if fingerprint != imported.fingerprint() {
                return Err(InstanceError::Conflict);
            }
            instance.id = id;
        }
        let id = instance.id.clone();
        let PendingAdmission { pin, lease } = match self.retained_admission(&id) {
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
        let service = self.for_operation(id.clone());
        let fallback = (id.clone(), pin.clone(), lease.clone());
        let task = self.tasks.try_spawn(
            (pin.clone(), lease.clone(), source_lease, imported.clone()),
            move |cancel| async move {
                tokio::task::spawn_blocking(move || {
                    service.import_admitted(instance, imported, pin, lease, cancel)
                })
                .await
                .unwrap_or_else(|error| {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(InstanceError::Cancelled)
                })
            },
        );
        if task.is_err() && self.creation_pending(&id).unwrap_or(true) {
            self.hold_admission(fallback.0, fallback.1, fallback.2);
        }
        task.map_err(|_| InstanceError::Closed)
    }

    fn import_admitted(
        &self,
        instance: Instance,
        imported: PreparedInstanceImport,
        pin: GenerationPin,
        lease: ExclusionLease,
        cancel: CancellationToken,
    ) -> InstanceResult<Instance> {
        let result = (|| {
            imported.revalidate().map_err(import_error)?;
            validate_destination_parent(&imported, &pin)?;
            self.settle_effects()?;
            let phase = self.creation_phase_for(&instance.id)?;
            if phase.as_deref() == Some("complete") {
                let record = self.registry().get_live(&instance.id)?;
                let admitted = super::directory::InstanceDirectories::admit_record(
                    self.registry().clone(),
                    record,
                    pin.clone(),
                    lease.clone(),
                )?;
                admitted.validate_current()?;
                self.verify_imported_history(&imported, &admitted.record().instance.id)?;
                return Ok(public_instance(admitted.record().instance.clone()));
            }
            if phase.as_deref().is_some_and(|phase| phase != "cancelled") {
                if let Some(recovered) =
                    self.recover_creation_with_import(&instance.id, &pin, Some(&imported))?
                {
                    return Ok(recovered);
                }
            }
            self.publish_instance(
                instance.clone(),
                pin.clone(),
                lease.clone(),
                Some(PublicationSource::Import(imported)),
                None,
                cancel,
            )
        })();
        if result.is_err() && self.creation_pending(&instance.id).unwrap_or(true) {
            self.hold_admission(instance.id, pin, lease);
        }
        result
    }

    fn creation_phase_for(&self, id: &InstanceId) -> InstanceResult<Option<String>> {
        self.registry().storage().read(|db| {
            Ok(db
                .query_row(
                    "SELECT phase FROM instance_creations WHERE instance_id=?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    fn creation_pending(&self, id: &InstanceId) -> InstanceResult<bool> {
        Ok(self
            .creation_phase_for(id)?
            .is_some_and(|phase| !matches!(phase.as_str(), "complete" | "cancelled")))
    }

    fn import_mapping(
        &self,
        imported: &PreparedInstanceImport,
    ) -> InstanceResult<Option<(InstanceId, String)>> {
        self.registry().storage().read(|db| {
            db.query_row(
                "SELECT instance_id,fingerprint FROM instance_imports WHERE source_id=?1 AND legacy_id=?2",
                params![imported.source_id(), imported.legacy_id()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            ).optional()?.map(|(id, fingerprint)| Ok((id.parse()?, fingerprint))).transpose()
        })
    }

    /// Explicit source-readmitted retries attest immutable history without
    /// comparing or recopying user-edited completed payloads.
    pub(crate) fn verify_imported_history(
        &self,
        imported: &PreparedInstanceImport,
        id: &InstanceId,
    ) -> InstanceResult<()> {
        let history = imported.bind_history(id).map_err(import_error)?;
        self.registry().storage().read(|db| {
            let transaction = db.unchecked_transaction()?;
            history
                .reports
                .verify_in(&transaction)
                .map_err(report_error)?;
            history
                .benchmarks
                .verify_in(&transaction)
                .map_err(benchmark_error)?;
            history
                .operations
                .verify_in(&transaction)
                .map_err(operation_error)?;
            history
                .installs
                .verify_in(&transaction)
                .map_err(install_history_error)?;
            if let Some(rules) = &history.rules {
                rules.verify_in(&transaction).map_err(rules_error)?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub(crate) fn reserve_import(
        &self,
        tx: &Transaction<'_>,
        imported: &PreparedInstanceImport,
        instance: Instance,
        pin: &GenerationPin,
    ) -> InstanceResult<InstanceRecord> {
        validate_destination(tx, imported)?;
        let mapping: Option<(String, String)> = tx.query_row(
            "SELECT instance_id,fingerprint FROM instance_imports WHERE source_id=?1 AND legacy_id=?2",
            params![imported.source_id(), imported.legacy_id()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if mapping.as_ref().is_some_and(|(id, fingerprint)| {
            id != instance.id.as_str() || fingerprint != imported.fingerprint()
        }) {
            return Err(InstanceError::Conflict);
        }
        let record = self
            .registry()
            .reserve(tx, instance, &pin.library_id().to_string())?;
        if mapping.is_none() {
            tx.execute(
                "INSERT INTO instance_imports(source_id,legacy_id,fingerprint,instance_id) VALUES(?1,?2,?3,?4)",
                params![imported.source_id(), imported.legacy_id(), imported.fingerprint(), record.instance.id.as_str()],
            )?;
        }
        Ok(record)
    }

    /// Recovery cannot reconstruct source authority from persisted text. A
    /// ready import remains reserved until the exact source is readmitted.
    pub(crate) fn verify_import_recovery(
        &self,
        id: &InstanceId,
        imported: Option<&PreparedInstanceImport>,
        directory: &ScopedDirectory,
    ) -> InstanceResult<()> {
        let mapping: Option<(String, String, String)> =
            self.registry().storage().read(|db| -> InstanceResult<_> {
                Ok(db.query_row(
                "SELECT source_id,legacy_id,fingerprint FROM instance_imports WHERE instance_id=?1",
                [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?)
            })?;
        let Some((source_id, legacy_id, fingerprint)) = mapping else {
            return Ok(());
        };
        let imported = imported.ok_or(InstanceError::SettlementRequired)?;
        if imported.source_id() != source_id
            || imported.legacy_id() != legacy_id
            || imported.fingerprint() != fingerprint
        {
            return Err(InstanceError::Conflict);
        }
        let record = self.registry().get_record(id)?;
        let mut expected = imported.instance().clone();
        expected.id = id.clone();
        expected.revision = 1;
        if record.instance != expected {
            return Err(InstanceError::Conflict);
        }
        // The durable ready point has already won cancellation. Complete only
        // after fresh byte verification against the admitted original preview.
        imported
            .verify_staged(directory, &CancellationToken::new())
            .map_err(import_error)?;
        imported.revalidate().map_err(import_error)
    }
}

struct CompletedMapping {
    source_id: String,
    legacy_id: String,
    id: InstanceId,
}

fn validate_mapping_digest(value: &str) -> InstanceResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(InstanceError::InvalidInput);
    }
    Ok(())
}

fn completed_mapping(row: &Row<'_>) -> InstanceResult<Option<CompletedMapping>> {
    let source_id: String = row
        .get::<_, Option<String>>(12)?
        .ok_or(InstanceError::InvalidInput)?;
    let fingerprint: String = row
        .get::<_, Option<String>>(13)?
        .ok_or(InstanceError::InvalidInput)?;
    validate_mapping_digest(&source_id)?;
    validate_mapping_digest(&fingerprint)?;
    let legacy_id: String = row.get(0)?;
    if !crate::import::model::legacy_id(&legacy_id) {
        return Err(InstanceError::InvalidInput);
    }
    let id: InstanceId = row.get::<_, String>(1)?.parse()?;
    let phase: Option<String> = row.get(2)?;
    let lifecycle: Option<String> = row.get(5)?;
    if phase.as_deref().is_some_and(|value| {
        !matches!(
            value,
            "building" | "ready" | "published" | "cancelling" | "complete" | "cancelled"
        )
    }) || lifecycle
        .as_deref()
        .is_some_and(|value| !matches!(value, "reserved" | "live" | "deleting"))
    {
        return Err(InstanceError::InvalidInput);
    }
    if phase.as_deref() != Some("complete") || lifecycle.as_deref() != Some("live") {
        return Ok(None);
    }
    let created = mapping_record(row.get(3)?, &id)?;
    let current = current_mapping_record(row, &id)?;
    let creation_receipt: Option<String> = row.get(4)?;
    if created.lifecycle != super::model::InstanceLifecycle::Reserved
        || created.directory_receipt.is_some()
        || created.revision != 1
        || current.revision <= created.revision
        || created.library_id != current.library_id
        || creation_receipt.as_deref().is_none_or(str::is_empty)
        || creation_receipt != current.directory_receipt
    {
        return Err(InstanceError::InvalidInput);
    }
    Ok(Some(CompletedMapping {
        source_id,
        legacy_id,
        id,
    }))
}

fn current_mapping_record(row: &Row<'_>, id: &InstanceId) -> InstanceResult<InstanceRecord> {
    let current = mapping_record(row.get(6)?, id)?;
    if current.lifecycle.as_str() != row.get::<_, String>(5)?
        || current.directory_receipt != row.get::<_, Option<String>>(7)?
        || current.library_id != row.get::<_, String>(8)?
        || current.directory_name != row.get::<_, String>(9)?
        || current.revision != row.get::<_, u64>(10)?
        || current.instance.name != row.get::<_, String>(11)?
    {
        return Err(InstanceError::InvalidInput);
    }
    if current.lifecycle != super::model::InstanceLifecycle::Live {
        return Err(InstanceError::Busy);
    }
    Ok(current)
}

fn mapping_record(raw: Option<String>, id: &InstanceId) -> InstanceResult<InstanceRecord> {
    let record = super::directory::decode(&raw.ok_or(InstanceError::InvalidInput)?)?;
    if record.instance.id != *id {
        return Err(InstanceError::InvalidInput);
    }
    Ok(record)
}

pub(crate) fn report_error(error: crate::launch::reports::ReportError) -> InstanceError {
    match error {
        crate::launch::reports::ReportError::Invalid
        | crate::launch::reports::ReportError::TooLarge => InstanceError::InvalidInput,
        crate::launch::reports::ReportError::ConflictingSession => InstanceError::Conflict,
        crate::launch::reports::ReportError::Storage(error) => InstanceError::Storage(error),
    }
}

pub(crate) fn benchmark_error(
    error: crate::performance::benchmarks::BenchmarkError,
) -> InstanceError {
    use crate::performance::benchmarks::BenchmarkError;
    match error {
        BenchmarkError::Invalid => InstanceError::InvalidInput,
        BenchmarkError::ConflictingHistory => InstanceError::Conflict,
        BenchmarkError::Storage(error) => InstanceError::Storage(error),
        _ => InstanceError::SettlementRequired,
    }
}

pub(crate) fn operation_error(
    error: crate::performance::mutation::OperationImportError,
) -> InstanceError {
    use crate::performance::mutation::OperationImportError;
    match error {
        OperationImportError::Invalid => InstanceError::InvalidInput,
        OperationImportError::Conflict => InstanceError::Conflict,
        OperationImportError::Storage(error) => InstanceError::Storage(error),
    }
}

pub(crate) fn install_history_error(error: crate::install::history::HistoryError) -> InstanceError {
    use crate::install::history::HistoryError;
    match error {
        HistoryError::Invalid => InstanceError::InvalidInput,
        HistoryError::Conflict => InstanceError::Conflict,
        HistoryError::Storage(error) => InstanceError::Storage(error),
    }
}

pub(crate) fn rules_error(error: crate::performance::rules::RulesImportError) -> InstanceError {
    use crate::performance::rules::RulesImportError;
    match error {
        RulesImportError::Storage(error) => InstanceError::Storage(error),
        RulesImportError::Cancelled => InstanceError::Cancelled,
        RulesImportError::Invalid => InstanceError::InvalidInput,
        _ => InstanceError::Conflict,
    }
}

/// Inspect the concrete instances parent before any import mutation. A parent
/// mounted through another namespace can alias a captured source even when the
/// two library roots have disjoint retained ancestry. This checks the admitted
/// roots and existing parent; it does not attest arbitrary future mount changes.
pub(crate) fn validate_destination_parent(
    imported: &PreparedInstanceImport,
    pin: &GenerationPin,
) -> InstanceResult<()> {
    let root = pin.files().map_err(|_| InstanceError::LibraryUnavailable)?;
    imported
        .validate_destination_root(root.capability())
        .map_err(import_error)?;
    match root.open_directory(&PortableName::new_exact("instances").expect("fixed portable name")) {
        Ok(parent) => imported
            .validate_destination_root(parent.capability())
            .map_err(import_error),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(InstanceError::DirectoryUnavailable),
    }
}

pub(crate) fn validate_destination(
    tx: &Transaction<'_>,
    imported: &PreparedInstanceImport,
) -> InstanceResult<()> {
    let config =
        crate::settings::SettingsStore::current_in_transaction(tx).map_err(
            |error| match error {
                crate::settings::SettingsError::Storage(error) => InstanceError::Storage(error),
                _ => InstanceError::InvalidSettings,
            },
        )?;
    imported
        .validate_destination(&config)
        .map_err(|_| InstanceError::InvalidSettings)
}

pub(crate) fn import_error(error: ImportError) -> InstanceError {
    match error {
        ImportError::Cancelled => InstanceError::Cancelled,
        ImportError::SourceChanged => InstanceError::Conflict,
        ImportError::InvalidData | ImportError::LimitExceeded => InstanceError::InvalidInput,
        _ => InstanceError::DirectoryUnavailable,
    }
}

pub(crate) fn copy_payload(
    service: &InstanceService,
    imported: &PreparedInstanceImport,
    destination: &ScopedDirectory,
    cancel: &CancellationToken,
) -> InstanceResult<()> {
    copy_tree(
        service,
        imported.source(),
        destination,
        cancel,
        &mut CopyBudget {
            entries: 1_000_000,
            bytes: 256 * 1024 * 1024 * 1024,
            file_bytes: 256 * 1024 * 1024 * 1024,
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        import::{
            Inventory,
            tests::{Fixture, snapshot},
        },
        instances::directory::{InstanceDirectories, Registry},
        launch::reports::{LaunchProofRecord, LaunchReportStore},
        library::{LibraryId, LibraryLifecycle, LibraryOpenOutcome},
        storage::MetadataStore,
        tasks::{Exclusions, TaskOwner},
    };
    use std::{path::Path, sync::Arc};

    fn add_history(source: &Fixture, instance_id: &str, session_id: &str) {
        let path = source.baseline.join("benchmarks/launch");
        std::fs::create_dir_all(&path).unwrap();
        let suite_id = format!("suite-dev-{instance_id}");
        let driver_id = format!("benchmark-suite-driver-{instance_id}");
        let benchmark_id = format!("benchmark-{instance_id}");
        let report = serde_json::json!({
            "schema":"axial.launch.proof", "schema_version":3, "session_id":session_id,
            "instance_id":instance_id, "version_id":"1.21.1",
            "launched_at":"2026-01-01T00:00:00.000Z", "recorded_at":"2026-01-01T00:00:02.000Z",
            "outcome":"exited", "session_outcome":{"reason":"clean_exit","kind":"clean","summary":"Minecraft exited cleanly."},
            "scenario":{"scenario_id":"vanilla_launch","performance_mode":"vanilla","requested_memory_mb":1024,"version_id":"1.21.1",
                "benchmark_profile":"vanilla_baseline","benchmark_run_type":"coldish","benchmark_mode":"development","benchmark_id":benchmark_id},
            "device":{"tier":"mid","total_memory_mb":8192,"cpu_threads":8},
            "exit_code":0,"boot_duration_ms":1000,"stages":[]
        });
        std::fs::write(
            path.join(format!("{session_id}.json")),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let suite = serde_json::json!({
            "schema":"axial.launch.benchmark.suite", "schema_version":2,
            "suite_id":suite_id, "instance_id":instance_id, "mode":"development",
            "created_at":"2026-01-01T00:00:00.000Z", "updated_at":"2026-01-01T00:00:02.000Z",
            "runs":[{"run_index":0,"profile":"vanilla_baseline","run_type":"coldish","target_id":"",
                "benchmark_id":benchmark_id,"session_id":session_id,"launched_at":"2026-01-01T00:00:00.000Z","state":"exited"}]
        });
        let driver = serde_json::json!({
            "id":driver_id,"suite_id":suite_id,"mode":"development","state":"stopped",
            "interval_ms":30000,"run_count":1,"launched_run_count":1,
            "pending_run_index":null,"active_session_id":null,"last_run_index":0,"last_session_id":session_id,
            "error":null,"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:02.000Z"
        });
        for (relative, value) in [
            (format!("benchmarks/suites/{suite_id}.json"), suite),
            (format!("benchmarks/suite-drivers/{driver_id}.json"), driver),
        ] {
            let path = source.baseline.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
    }

    fn add_install_history(source: &Fixture) {
        let directory = source.baseline.join("state");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("operation-journals.json"),
            serde_json::to_vec(&crate::import::tests::successful_install_journal()).unwrap(),
        )
        .unwrap();
    }

    fn install_rows(service: &InstanceService) -> Vec<(String, String, Option<String>, Vec<u8>)> {
        service
            .registry()
            .storage()
            .read(|db| -> InstanceResult<_> {
                let mut query = db.prepare(
                    "SELECT id,source_id,instance_id,payload FROM install_history ORDER BY id",
                )?;
                Ok(query
                    .query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<Result<_, _>>()?)
            })
            .unwrap()
    }

    fn assert_install_history(service: &InstanceService, id: &InstanceId) {
        let page = service.imported_install_history(id, None).unwrap();
        assert_eq!(page.records.len(), 3);
        assert!(page.next_after.is_none());
        assert!(page.records.iter().all(|record| record.historical));
        assert_eq!(
            page.records
                .iter()
                .filter(|record| record.instance_id.is_none())
                .count(),
            2
        );
        let content = page
            .records
            .iter()
            .find(|record| record.instance_id.is_some())
            .unwrap();
        assert_eq!(content.instance_id.as_deref(), Some(id.as_str()));
        assert_eq!(content.command, "ModifyInstanceContent");
        assert_eq!(content.sequence, "13");
    }

    fn reports(root: &Path) -> Vec<LaunchProofRecord> {
        LaunchReportStore::new(Arc::new(
            MetadataStore::open(root.join("metadata.sqlite")).unwrap(),
        ))
        .unwrap()
        .list_recent(25)
        .unwrap()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct StoredBenchmarkHistory {
        suites: Vec<(String, Vec<u8>)>,
        drivers: Vec<(String, Vec<u8>, Option<Vec<u8>>)>,
    }

    fn benchmark_history(root: &Path) -> StoredBenchmarkHistory {
        MetadataStore::open(root.join("metadata.sqlite"))
            .unwrap()
            .read(|db| -> InstanceResult<_> {
                let mut suites =
                    db.prepare("SELECT suite_id,payload FROM benchmark_suites ORDER BY suite_id")?;
                let suites = suites
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?;
                let mut drivers = db.prepare(
                    "SELECT driver_id,payload,request FROM benchmark_drivers ORDER BY driver_id",
                )?;
                let drivers = drivers
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<_, _>>()?;
                Ok(StoredBenchmarkHistory { suites, drivers })
            })
            .unwrap()
    }

    fn assert_benchmark_history(
        history: &StoredBenchmarkHistory,
        id: &InstanceId,
        report: &LaunchProofRecord,
    ) {
        assert_eq!(history.suites.len(), 1);
        assert_eq!(history.drivers.len(), 1);
        let suite: serde_json::Value = serde_json::from_slice(&history.suites[0].1).unwrap();
        let driver: serde_json::Value = serde_json::from_slice(&history.drivers[0].1).unwrap();
        assert_eq!(suite["suite_id"], history.suites[0].0);
        assert_eq!(suite["instance_id"], id.as_str());
        assert_eq!(suite["historical"], true);
        assert_eq!(suite["runs"][0]["session_id"], report.session_id);
        assert_eq!(
            suite["runs"][0]["benchmark_id"],
            report.scenario.benchmark_id.as_deref().unwrap()
        );
        assert!(suite["runs"][0].get("launch_intent").is_none());
        assert_eq!(driver["id"], history.drivers[0].0);
        assert_eq!(driver["suite_id"], suite["suite_id"]);
        assert_eq!(driver["last_session_id"], report.session_id);
        assert_eq!(driver["historical"], true);
        assert_eq!(driver["state"], "stopped");
        assert!(driver["active_session_id"].is_null());
        assert!(history.drivers[0].2.is_none());
    }

    fn prepared(source: &Fixture) -> PreparedInstanceImport {
        let inventory = Arc::new(source.capture());
        Inventory::prepare_instance(
            &inventory,
            &inventory.preview().fingerprint,
            "0000000000000001",
        )
        .unwrap()
    }

    fn reopen(root: &Path, library_id: LibraryId) -> InstanceService {
        let library = match LibraryLifecycle::open_with_id(root, library_id) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated restarted library: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
        InstanceService::new(
            InstanceDirectories::new(Registry::new(storage), library, Exclusions::new()),
            TaskOwner::new(16).unwrap(),
        )
    }

    /// Materialize the actual reservation/stage witnesses at a crash boundary.
    /// No source paths or synthetic receipts are placed in recovery metadata.
    fn interrupted(
        service: &InstanceService,
        imported: &PreparedInstanceImport,
        phase: &str,
    ) -> InstanceId {
        let pin = service.directories.library().admit().unwrap();
        let id = imported.instance().id.clone();
        let stage_name = format!("stage-{id}");
        let record = service.registry().storage().transaction(|tx| -> InstanceResult<_> {
            let record = service.reserve_import(tx, imported, imported.instance().clone(), &pin)?;
            tx.execute(
                "INSERT INTO instance_creations(instance_id,record_json,stage_name,phase) VALUES(?1,?2,?3,'building')",
                params![id.as_str(), serde_json::to_string(&record).unwrap(), stage_name],
            )?;
            Ok(record)
        }).unwrap();
        let parent = service.ensure_parent(&pin).unwrap();
        let stage = service
            .fresh_directory(&parent, &PortableName::new_exact(&stage_name).unwrap())
            .unwrap();
        let receipt = stage.receipt().unwrap();
        if phase != "building" {
            copy_payload(service, imported, &stage, &CancellationToken::new()).unwrap();
            imported
                .verify_staged(&stage, &CancellationToken::new())
                .unwrap();
        }
        service
            .registry()
            .storage()
            .transaction(|tx| -> InstanceResult<()> {
                tx.execute(
                "UPDATE instance_creations SET phase=?2,directory_receipt=?3 WHERE instance_id=?1",
                params![id.as_str(), phase, receipt],
            )?;
                Ok(())
            })
            .unwrap();
        if phase == "published" {
            let (outcome, _pin) = stage
                .move_no_replace(
                    &parent,
                    &PortableName::new_exact(&record.directory_name).unwrap(),
                )
                .into_parts();
            assert!(matches!(
                outcome,
                axial_fs::DirectoryMoveOutcome::Applied(_)
            ));
        }
        id
    }

    #[tokio::test]
    async fn completed_import_mappings_survive_reopen_without_target_exclusion_or_name_inference() {
        let source = Fixture::new();
        let imported = prepared(&source);
        let (root, service) = super::super::create::tests::fixture();
        assert!(
            service
                .completed_import_mappings(imported.source_id(), imported.fingerprint())
                .unwrap()
                .is_empty()
        );
        let instance = service
            .import_instance(imported.clone())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        service
            .update(
                &instance.id,
                super::super::model::InstancePatch {
                    name: Some("User renamed this".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let expected = BTreeMap::from([(imported.legacy_id().to_owned(), instance.id.to_string())]);
        let retained = service.directories.admit(&instance.id).unwrap();
        assert_eq!(
            service
                .completed_import_mappings(imported.source_id(), imported.fingerprint())
                .unwrap(),
            expected
        );
        drop(retained);
        let library_id = service.directories.library().admit().unwrap().library_id();
        drop(service);
        let service = reopen(root.path(), library_id);
        assert_eq!(
            service
                .completed_import_mappings(imported.source_id(), imported.fingerprint())
                .unwrap(),
            expected
        );
        assert!(
            service
                .completed_import_mappings(imported.source_id(), &"0".repeat(64))
                .unwrap()
                .is_empty()
        );
        assert!(
            service
                .completed_import_mappings(&"0".repeat(64), imported.fingerprint())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn completed_import_mappings_exclude_every_pending_and_cancelled_publication() {
        for phase in ["building", "ready", "published", "cancelling", "cancelled"] {
            let source = Fixture::new();
            let imported = prepared(&source);
            let (_root, service) = super::super::create::tests::fixture();
            interrupted(&service, &imported, phase);
            let before = service.registry().pending().unwrap();
            assert!(
                service
                    .completed_import_mappings(imported.source_id(), imported.fingerprint())
                    .unwrap()
                    .is_empty(),
                "{phase}"
            );
            assert_eq!(service.registry().pending().unwrap(), before);
            assert_eq!(
                service
                    .creation_phase_for(&imported.instance().id)
                    .unwrap()
                    .as_deref(),
                Some(phase)
            );
        }
    }

    #[tokio::test]
    async fn completed_import_mappings_exclude_deleted_and_missing_registration_evidence() {
        for removed in ["keep_files", "delete_files", "registry", "creation"] {
            let source = Fixture::new();
            let imported = prepared(&source);
            let (_root, service) = super::super::create::tests::fixture();
            let instance = service
                .import_instance(imported.clone())
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            if matches!(removed, "keep_files" | "delete_files") {
                let intent = if removed == "keep_files" {
                    super::super::delete::DeleteIntent::KeepFiles
                } else {
                    super::super::delete::DeleteIntent::DeleteFiles
                };
                service
                    .delete(&instance.id, intent, uuid::Uuid::new_v4())
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                service
                    .registry()
                    .storage()
                    .transaction(|tx| -> InstanceResult<()> {
                        let sql = if removed == "registry" {
                            "DELETE FROM instances WHERE id=?1"
                        } else {
                            "DELETE FROM instance_creations WHERE instance_id=?1"
                        };
                        tx.execute(sql, [instance.id.as_str()])?;
                        Ok(())
                    })
                    .unwrap();
            }
            assert!(
                service
                    .completed_import_mappings(imported.source_id(), imported.fingerprint())
                    .unwrap()
                    .is_empty(),
                "{removed}"
            );
            assert_eq!(
                service.import_mapping(&imported).unwrap().unwrap().0,
                instance.id
            );
        }
    }

    #[tokio::test]
    async fn completed_import_mappings_reject_malformed_durable_identity_without_repair() {
        for sql in [
            "UPDATE instance_imports SET legacy_id='../source'",
            "UPDATE instance_imports SET instance_id='not-an-id'",
            "UPDATE instance_creations SET record_json='{}'",
            "UPDATE instance_creations SET directory_receipt=NULL",
            "UPDATE instances SET record_json='{}'",
            "UPDATE instances SET directory_name='unrelated'",
            "UPDATE instances SET revision=999",
        ] {
            let source = Fixture::new();
            let imported = prepared(&source);
            let (_root, service) = super::super::create::tests::fixture();
            service
                .import_instance(imported.clone())
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            service
                .registry()
                .storage()
                .transaction(|tx| -> InstanceResult<()> {
                    tx.execute_batch(sql)?;
                    Ok(())
                })
                .unwrap();
            assert!(
                service
                    .completed_import_mappings(imported.source_id(), imported.fingerprint())
                    .is_err(),
                "{sql}"
            );
        }
    }

    #[tokio::test]
    async fn ready_and_promoted_imports_require_source_readmission_after_restart() {
        for phase in ["ready", "published"] {
            let source = Fixture::new();
            add_history(&source, "0000000000000001", "session-a");
            add_install_history(&source);
            let (root, service) = super::super::create::tests::fixture();
            let id = interrupted(&service, &prepared(&source), phase);
            let library_id = service.directories.library().admit().unwrap().library_id();
            assert!(service.registry().list().unwrap().is_empty());
            assert!(reports(root.path()).is_empty());
            assert!(install_rows(&service).is_empty());
            let pending_benchmarks = benchmark_history(root.path());
            assert!(pending_benchmarks.suites.is_empty());
            assert!(pending_benchmarks.drivers.is_empty());
            drop(service);
            let service = reopen(root.path(), library_id);
            assert!(matches!(
                service.recover_creation(&id).unwrap().join().await.unwrap(),
                Err(InstanceError::SettlementRequired)
            ));
            assert!(service.registry().list().unwrap().is_empty());
            assert_eq!(benchmark_history(root.path()), pending_benchmarks);
            let imported = service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(imported.id, id);
            assert_eq!(service.registry().list().unwrap().len(), 1);
            assert!(service.pending().unwrap().is_empty());
            let imported_reports = reports(root.path());
            assert_eq!(imported_reports.len(), 1);
            assert_eq!(imported_reports[0].instance_id, id.as_str());
            assert_benchmark_history(&benchmark_history(root.path()), &id, &imported_reports[0]);
            assert_install_history(&service, &id);
            assert!(!service.has_unsettled_effects());
            service
                .directories
                .admit(&id)
                .unwrap()
                .validate_current()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn changed_ready_bytes_remain_reserved_and_preserved_after_restart() {
        let source = Fixture::new();
        let (root, service) = super::super::create::tests::fixture();
        let id = interrupted(&service, &prepared(&source), "ready");
        let library_id = service.directories.library().admit().unwrap().library_id();
        let stage_path = service
            .directories
            .library()
            .admit()
            .unwrap()
            .read_projection()
            .unwrap()
            .join("instances")
            .join(format!("stage-{id}"));
        std::fs::write(stage_path.join("unexpected-user-file"), b"preserve this").unwrap();
        drop(service);
        let service = reopen(root.path(), library_id);
        assert!(matches!(
            service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(InstanceError::InvalidInput)
        ));
        assert!(service.registry().list().unwrap().is_empty());
        assert_eq!(service.pending().unwrap()[0].instance_id, id);
        assert_eq!(
            std::fs::read(stage_path.join("unexpected-user-file")).unwrap(),
            b"preserve this"
        );
        assert!(!stage_path.parent().unwrap().join(id.as_str()).exists());
    }

    #[tokio::test]
    async fn cancellation_releases_building_stage_and_retry_reuses_reserved_mapping() {
        let source = Fixture::new();
        add_history(&source, "0000000000000001", "session-a");
        add_install_history(&source);
        let (root, service) = super::super::create::tests::fixture();
        let imported = prepared(&source);
        let id = interrupted(&service, &imported, "building");
        let pin = service.directories.library().admit().unwrap();
        let lease = service
            .directories
            .exclusions()
            .try_acquire([id.as_str()], [])
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = service.for_operation(id.clone()).import_admitted(
            imported.instance().clone(),
            imported,
            pin,
            lease,
            cancel,
        );
        assert!(matches!(result, Err(InstanceError::Cancelled)));
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.registry().pending().unwrap().is_empty());
        assert!(service.pending().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
        assert!(reports(root.path()).is_empty());
        assert!(benchmark_history(root.path()).suites.is_empty());
        assert!(benchmark_history(root.path()).drivers.is_empty());
        assert!(install_rows(&service).is_empty());
        let instance = service
            .import_instance(prepared(&source))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.id, id);
        assert_install_history(&service, &id);
        assert_eq!(reports(root.path())[0].instance_id, id.as_str());
        assert_benchmark_history(
            &benchmark_history(root.path()),
            &id,
            &reports(root.path())[0],
        );
    }

    #[tokio::test]
    async fn cancellation_before_reservation_has_no_mapping_or_payload_effect() {
        let source = Fixture::new();
        let (_root, service) = super::super::create::tests::fixture();
        let imported = prepared(&source);
        let pin = service.directories.library().admit().unwrap();
        let destination = pin.read_projection().unwrap();
        let lease = service
            .directories
            .exclusions()
            .try_acquire([imported.instance().id.as_str()], [])
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            service.import_admitted(
                imported.instance().clone(),
                imported.clone(),
                pin,
                lease,
                cancel,
            ),
            Err(InstanceError::Cancelled)
        ));
        assert!(service.import_mapping(&imported).unwrap().is_none());
        assert!(service.registry().pending().unwrap().is_empty());
        assert!(!destination.join("instances").exists());
    }

    #[tokio::test]
    async fn destination_settings_change_cannot_publish_an_incompatible_ready_import() {
        let source = Fixture::new();
        let (root, service) = super::super::create::tests::fixture();
        let id = interrupted(&service, &prepared(&source), "ready");
        let settings = crate::settings::SettingsStore::new(Arc::new(
            MetadataStore::open(root.path().join("metadata.sqlite")).unwrap(),
        ))
        .unwrap();
        settings
            .update(crate::settings::ConfigPatch {
                jvm_preset: Some(crate::settings::ConfigJvmPreset::Smooth),
                ..Default::default()
            })
            .unwrap();
        assert!(matches!(
            service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(InstanceError::InvalidSettings)
        ));
        assert!(service.registry().list().unwrap().is_empty());
        assert_eq!(service.pending().unwrap()[0].instance_id, id);
        settings
            .update(crate::settings::ConfigPatch {
                expected_revision: settings.current().unwrap().revision,
                jvm_preset: Some(crate::settings::ConfigJvmPreset::Automatic),
                ..Default::default()
            })
            .unwrap();
        let imported = service
            .import_instance(prepared(&source))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(imported.id, id);
        assert!(service.pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn final_creation_write_cannot_acknowledge_corrupted_history() {
        for (table, phase) in ["launch_reports", "benchmark_suites", "benchmark_drivers"]
            .into_iter()
            .flat_map(|table| ["initial", "ready", "published"].map(|phase| (table, phase)))
        {
            let source = Fixture::new();
            add_history(&source, "0000000000000001", "session-a");
            let (root, service) = super::super::create::tests::fixture();
            let imported = prepared(&source);
            let id = if phase == "initial" {
                imported.instance().id.clone()
            } else {
                interrupted(&service, &imported, phase)
            };
            service.registry().storage().transaction(|tx| -> InstanceResult<()> {
                tx.execute_batch(&format!("CREATE TRIGGER corrupt_final_history AFTER UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN DELETE FROM {table}; END;"))?;
                Ok(())
            }).unwrap();
            assert!(
                matches!(
                    service
                        .import_instance(imported)
                        .unwrap()
                        .join()
                        .await
                        .unwrap(),
                    Err(InstanceError::Conflict)
                ),
                "{table}/{phase}"
            );
            assert!(service.registry().list().unwrap().is_empty());
            assert_eq!(service.pending().unwrap()[0].instance_id, id);
            assert!(reports(root.path()).is_empty());
            assert!(benchmark_history(root.path()).suites.is_empty());
            assert!(benchmark_history(root.path()).drivers.is_empty());
            service
                .registry()
                .storage()
                .transaction(|tx| -> InstanceResult<()> {
                    tx.execute_batch("DROP TRIGGER corrupt_final_history")?;
                    Ok(())
                })
                .unwrap();
            let recovered = service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(recovered.id, id);
            assert_eq!(reports(root.path()).len(), 1);
            assert_benchmark_history(
                &benchmark_history(root.path()),
                &id,
                &reports(root.path())[0],
            );
        }
    }

    #[tokio::test]
    async fn history_and_visibility_roll_back_together_then_recover_with_reserved_id() {
        for phase in ["initial", "ready", "published"] {
            let source = Fixture::new();
            add_history(&source, "0000000000000001", "session-a");
            add_install_history(&source);
            let source_before = snapshot(&source.baseline);
            let (root, service) = super::super::create::tests::fixture();
            let imported = prepared(&source);
            let id = if phase == "initial" {
                imported.instance().id.clone()
            } else {
                interrupted(&service, &imported, phase)
            };
            service.registry().storage().transaction(|tx| -> InstanceResult<()> {
                // This fires after report, suite, driver AND registry writes in
                // both final transaction paths, exercising their real rollback.
                tx.execute_batch("CREATE TRIGGER reject_history_commit BEFORE UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(ABORT, 'injected history commit failure'); END;")?;
                Ok(())
            }).unwrap();
            assert!(matches!(
                service
                    .import_instance(imported)
                    .unwrap()
                    .join()
                    .await
                    .unwrap(),
                Err(InstanceError::Storage(_))
            ));
            assert!(service.registry().list().unwrap().is_empty());
            assert_eq!(service.pending().unwrap()[0].instance_id, id);
            assert!(reports(root.path()).is_empty());
            assert!(benchmark_history(root.path()).suites.is_empty());
            assert!(benchmark_history(root.path()).drivers.is_empty());
            assert!(install_rows(&service).is_empty());
            assert_eq!(snapshot(&source.baseline), source_before);
            service
                .registry()
                .storage()
                .transaction(|tx| -> InstanceResult<()> {
                    tx.execute_batch("DROP TRIGGER reject_history_commit")?;
                    Ok(())
                })
                .unwrap();
            let library_id = service.directories.library().admit().unwrap().library_id();
            drop(service);
            let service = reopen(root.path(), library_id);
            let readmitted = prepared(&source);
            assert_ne!(
                readmitted.instance().id,
                id,
                "readmission produces a new unreserved candidate"
            );
            let recovered = service
                .import_instance(readmitted)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(recovered.id, id);
            assert_eq!(service.registry().list().unwrap().len(), 1);
            assert!(service.pending().unwrap().is_empty());
            let saved = reports(root.path());
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].schema_version, 4);
            assert_eq!(saved[0].instance_id, id.as_str());
            assert_eq!(saved[0].boot_duration_ms, Some(1000));
            assert_eq!(saved[0].recorded_at, "2026-01-01T00:00:02.000Z");
            assert_benchmark_history(&benchmark_history(root.path()), &id, &saved[0]);
            assert_install_history(&service, &id);
            assert_eq!(snapshot(&source.baseline), source_before);
        }
    }

    #[tokio::test]
    async fn install_history_refuses_ignored_or_conflicting_rows_before_publication() {
        for phase in ["initial", "ready", "published"] {
            for fault in ["ignored_insert", "conflicting_row"] {
                let source = Fixture::new();
                add_history(&source, "0000000000000001", "session-a");
                add_install_history(&source);
                let source_before = snapshot(&source.baseline);
                let (_root, service) = super::super::create::tests::fixture();
                let imported = prepared(&source);
                let id = if phase == "initial" {
                    imported.instance().id.clone()
                } else {
                    interrupted(&service, &imported, phase)
                };
                let history = imported.bind_history(&id).unwrap();
                service.registry().storage().transaction(|tx| -> InstanceResult<()> {
                    if fault == "ignored_insert" {
                        tx.execute_batch("CREATE TRIGGER ignore_install_history BEFORE INSERT ON install_history BEGIN SELECT RAISE(IGNORE); END;")?;
                    } else {
                        history.installs.insert_in(tx).map_err(install_history_error)?;
                        let (key, payload): (String, Vec<u8>) = tx.query_row(
                            "SELECT id,payload FROM install_history WHERE instance_id IS NOT NULL", [],
                            |row| Ok((row.get(0)?, row.get(1)?)))?;
                        let mut changed: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                        changed["source"]["sequence"] = serde_json::json!(99);
                        tx.execute("UPDATE install_history SET payload=?1 WHERE id=?2",
                            params![serde_json::to_vec(&changed).unwrap(), key])?;
                    }
                    Ok(())
                }).unwrap();
                let stored_before = install_rows(&service);
                let result = service
                    .import_instance(imported)
                    .unwrap()
                    .join()
                    .await
                    .unwrap();
                assert!(
                    matches!(result, Err(InstanceError::Conflict)),
                    "{phase}/{fault}"
                );
                assert!(service.registry().list().unwrap().is_empty());
                assert_eq!(service.pending().unwrap()[0].instance_id, id);
                assert_eq!(install_rows(&service), stored_before);
                assert_eq!(snapshot(&source.baseline), source_before);
                service
                    .registry()
                    .storage()
                    .transaction(|tx| -> InstanceResult<()> {
                        if fault == "ignored_insert" {
                            tx.execute_batch("DROP TRIGGER ignore_install_history")?;
                        } else {
                            tx.execute("DELETE FROM install_history", [])?;
                        }
                        Ok(())
                    })
                    .unwrap();
                let recovered = service
                    .import_instance(prepared(&source))
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(recovered.id, id);
                assert_install_history(&service, &id);
                assert_eq!(snapshot(&source.baseline), source_before);
            }
        }
    }

    #[tokio::test]
    async fn conflicting_benchmark_history_rolls_back_publication_without_overwriting_evidence() {
        for phase in ["initial", "ready", "published"] {
            for conflict in ["suite", "driver"] {
                let source = Fixture::new();
                add_history(&source, "0000000000000001", "session-a");
                let source_before = snapshot(&source.baseline);
                let (root, service) = super::super::create::tests::fixture();
                let imported = prepared(&source);
                let id = if phase == "initial" {
                    imported.instance().id.clone()
                } else {
                    interrupted(&service, &imported, phase)
                };
                let bound = imported.bind_history(&id).unwrap();
                service
                    .registry()
                    .storage()
                    .transaction(|tx| -> InstanceResult<()> {
                        bound.benchmarks.insert_in(tx).map_err(benchmark_error)
                    })
                    .unwrap();
                let expected = benchmark_history(root.path());
                service
                    .registry()
                    .storage()
                    .transaction(|tx| -> InstanceResult<()> {
                        if conflict == "suite" {
                            let mut changed: serde_json::Value =
                                serde_json::from_slice(&expected.suites[0].1).unwrap();
                            changed["updated_at"] = serde_json::json!("2026-01-01T00:00:03.000Z");
                            tx.execute(
                                "UPDATE benchmark_suites SET payload=?1 WHERE suite_id=?2",
                                params![
                                    serde_json::to_vec(&changed).unwrap(),
                                    expected.suites[0].0
                                ],
                            )?;
                            tx.execute("DELETE FROM benchmark_drivers", [])?;
                        } else {
                            let mut changed: serde_json::Value =
                                serde_json::from_slice(&expected.drivers[0].1).unwrap();
                            changed["updated_at"] = serde_json::json!("2026-01-01T00:00:03.000Z");
                            tx.execute(
                                "UPDATE benchmark_drivers SET payload=?1 WHERE driver_id=?2",
                                params![
                                    serde_json::to_vec(&changed).unwrap(),
                                    expected.drivers[0].0
                                ],
                            )?;
                            tx.execute("DELETE FROM benchmark_suites", [])?;
                        }
                        Ok(())
                    })
                    .unwrap();
                let conflicting = benchmark_history(root.path());
                assert!(
                    matches!(
                        service
                            .import_instance(imported)
                            .unwrap()
                            .join()
                            .await
                            .unwrap(),
                        Err(InstanceError::Conflict)
                    ),
                    "{phase}: {conflict}"
                );
                assert!(service.registry().list().unwrap().is_empty());
                assert_eq!(service.pending().unwrap()[0].instance_id, id);
                assert!(reports(root.path()).is_empty());
                assert_eq!(benchmark_history(root.path()), conflicting);
                assert_eq!(snapshot(&source.baseline), source_before);
            }
        }
    }

    #[tokio::test]
    async fn completed_retry_verifies_history_without_recopy_or_repairing_missing_evidence() {
        for corruption in [
            "missing",
            "payload",
            "indexed_instance",
            "missing_suite",
            "suite_payload",
            "missing_driver",
            "driver_payload",
            "missing_install",
            "install_payload",
            "install_index",
        ] {
            let source = Fixture::new();
            add_history(&source, "0000000000000001", "session-a");
            add_install_history(&source);
            let before = snapshot(&source.baseline);
            let (root, service) = super::super::create::tests::fixture();
            let instance = service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            let path = root
                .path()
                .join("instances")
                .join(instance.id.as_str())
                .join("options.txt");
            std::fs::write(&path, b"later destination user edit").unwrap();
            let saved_benchmarks = benchmark_history(root.path());
            assert_benchmark_history(&saved_benchmarks, &instance.id, &reports(root.path())[0]);
            let repeated = service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(repeated.id, instance.id);
            assert_eq!(benchmark_history(root.path()), saved_benchmarks);
            assert_install_history(&service, &instance.id);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"later destination user edit"
            );
            let saved = reports(root.path());
            assert_eq!(saved.len(), 1);
            service.registry().storage().transaction(|tx| -> InstanceResult<()> {
                match corruption {
                    "missing" => { tx.execute("DELETE FROM launch_reports WHERE session_id=?1", [&saved[0].session_id])?; }
                    "payload" => {
                        let mut changed = saved[0].clone();
                        changed.boot_duration_ms = Some(999);
                        tx.execute("UPDATE launch_reports SET payload=?1 WHERE session_id=?2", params![serde_json::to_vec(&changed).unwrap(), changed.session_id])?;
                    }
                    "indexed_instance" => { tx.execute("UPDATE launch_reports SET instance_id='another-instance' WHERE session_id=?1", [&saved[0].session_id])?; }
                    "missing_suite" => { tx.execute("DELETE FROM benchmark_suites WHERE suite_id=?1", [&saved_benchmarks.suites[0].0])?; }
                    "suite_payload" => {
                        let mut changed: serde_json::Value = serde_json::from_slice(&saved_benchmarks.suites[0].1).unwrap();
                        changed["updated_at"] = serde_json::json!("2026-01-01T00:00:03.000Z");
                        tx.execute("UPDATE benchmark_suites SET payload=?1 WHERE suite_id=?2", params![serde_json::to_vec(&changed).unwrap(), saved_benchmarks.suites[0].0])?;
                    }
                    "missing_driver" => { tx.execute("DELETE FROM benchmark_drivers WHERE driver_id=?1", [&saved_benchmarks.drivers[0].0])?; }
                    "driver_payload" => {
                        let mut changed: serde_json::Value = serde_json::from_slice(&saved_benchmarks.drivers[0].1).unwrap();
                        changed["updated_at"] = serde_json::json!("2026-01-01T00:00:03.000Z");
                        tx.execute("UPDATE benchmark_drivers SET payload=?1 WHERE driver_id=?2", params![serde_json::to_vec(&changed).unwrap(), saved_benchmarks.drivers[0].0])?;
                    }
                    "missing_install" => { tx.execute("DELETE FROM install_history", [])?; }
                    "install_payload" => {
                        tx.execute("UPDATE install_history SET payload=X'7b7d' WHERE instance_id IS NOT NULL", [])?;
                    }
                    "install_index" => {
                        tx.execute("UPDATE install_history SET instance_id=?1 WHERE instance_id IS NOT NULL", [InstanceId::new().as_str()])?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            }).unwrap();
            let corrupted_benchmarks = benchmark_history(root.path());
            let corrupted_installs = install_rows(&service);
            let result = service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap();
            assert!(matches!(
                result,
                Err(InstanceError::Conflict | InstanceError::InvalidInput)
            ));
            // The second explicit-complete branch must attest the same history.
            let pin = service.directories.library().admit().unwrap();
            assert!(
                service
                    .recover_creation_with_import(&instance.id, &pin, Some(&prepared(&source)))
                    .is_err()
            );
            // Ordinary source-free completed recovery is not an import attestation.
            assert!(
                service
                    .recover_creation_with_import(&instance.id, &pin, None)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(service.registry().list().unwrap().len(), 1);
            assert!(service.pending().unwrap().is_empty());
            assert_eq!(std::fs::read(path).unwrap(), b"later destination user edit");
            assert_eq!(snapshot(&source.baseline), before);
            assert_eq!(benchmark_history(root.path()), corrupted_benchmarks);
            assert_eq!(install_rows(&service), corrupted_installs);
            if corruption == "missing" {
                assert!(reports(root.path()).is_empty());
            }
        }
    }

    #[tokio::test]
    async fn install_history_reader_requires_live_completed_mapping_and_owner_cursor() {
        let (_root, service) = super::super::create::tests::fixture();
        assert!(matches!(
            service.imported_install_history(&InstanceId::new(), None),
            Err(InstanceError::NotFound)
        ));
        let ordinary = super::super::create::tests::create(&service, "Not imported").await;
        assert!(
            service
                .imported_install_history(&ordinary.id, None)
                .unwrap()
                .records
                .is_empty()
        );
        for after in ["", "invalid", "legacy-install-not-a-digest"] {
            assert!(matches!(
                service.imported_install_history(&ordinary.id, Some(after)),
                Err(InstanceError::InvalidInput)
            ));
        }
        let source = Fixture::new();
        add_install_history(&source);
        let id = interrupted(&service, &prepared(&source), "ready");
        assert!(matches!(
            service.imported_install_history(&id, None),
            Err(InstanceError::Busy)
        ));
        service
            .import_instance(prepared(&source))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_install_history(&service, &id);
        let held = service.directories.admit(&id).unwrap();
        assert_install_history(&service, &id);
        drop(held);
        let page = service.imported_install_history(&id, None).unwrap();
        assert!(
            service
                .imported_install_history(&id, Some(&page.records.last().unwrap().id))
                .unwrap()
                .records
                .is_empty()
        );
        for sql in [
            "UPDATE instance_imports SET source_id='not-a-source'",
            "UPDATE instance_imports SET fingerprint='not-a-fingerprint'",
            "UPDATE instance_imports SET legacy_id='../source'",
            "UPDATE instance_creations SET phase='published'",
            "UPDATE instance_creations SET record_json='{}'",
            "UPDATE instance_creations SET directory_receipt=NULL",
            "UPDATE instances SET record_json='{}'",
            "UPDATE instances SET revision=999",
        ] {
            let source = Fixture::new();
            add_install_history(&source);
            let (_root, service) = super::super::create::tests::fixture();
            let imported = service
                .import_instance(prepared(&source))
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_install_history(&service, &imported.id);
            let rows_before = install_rows(&service);
            service
                .registry()
                .storage()
                .transaction(|tx| -> InstanceResult<()> {
                    tx.execute_batch(sql)?;
                    Ok(())
                })
                .unwrap();
            assert!(
                service
                    .imported_install_history(&imported.id, None)
                    .is_err(),
                "{sql}"
            );
            assert_eq!(install_rows(&service), rows_before);
        }
    }

    #[tokio::test]
    async fn publication_imports_only_selected_instances_history_and_repeat_is_immutable() {
        let source = Fixture::new();
        let registry_path = source.baseline.join("instances.json");
        let mut registry: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        let mut second = registry["instances"][0].clone();
        second["id"] = serde_json::json!("0000000000000002");
        second["name"] = serde_json::json!("Second imported instance");
        registry["instances"].as_array_mut().unwrap().push(second);
        std::fs::write(registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
        std::fs::create_dir(source.baseline.join("instances/0000000000000002")).unwrap();
        add_history(&source, "0000000000000001", "session-a");
        add_history(&source, "0000000000000002", "session-b");
        let before = snapshot(&source.baseline);
        let (root, service) = super::super::create::tests::fixture();
        let first = service
            .import_instance(prepared(&source))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let first_history = reports(root.path());
        assert_eq!(first_history.len(), 1);
        assert_eq!(first_history[0].instance_id, first.id.as_str());
        let first_benchmarks = benchmark_history(root.path());
        assert_benchmark_history(&first_benchmarks, &first.id, &first_history[0]);
        let inventory = Arc::new(source.capture());
        let second = inventory
            .prepare_instance(&inventory.preview().fingerprint, "0000000000000002")
            .unwrap();
        let second = service
            .import_instance(second)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let both = reports(root.path());
        assert_eq!(both.len(), 2);
        assert!(
            both.iter()
                .any(|report| report.instance_id == second.id.as_str())
        );
        assert_eq!(
            both.iter()
                .find(|report| report.instance_id == first.id.as_str()),
            first_history.first()
        );
        let both_benchmarks = benchmark_history(root.path());
        assert_eq!(both_benchmarks.suites.len(), 2);
        assert_eq!(both_benchmarks.drivers.len(), 2);
        assert!(both_benchmarks.suites.contains(&first_benchmarks.suites[0]));
        assert!(
            both_benchmarks
                .drivers
                .contains(&first_benchmarks.drivers[0])
        );
        let second_benchmarks = StoredBenchmarkHistory {
            suites: both_benchmarks
                .suites
                .iter()
                .filter(|row| !first_benchmarks.suites.contains(row))
                .cloned()
                .collect(),
            drivers: both_benchmarks
                .drivers
                .iter()
                .filter(|row| !first_benchmarks.drivers.contains(row))
                .cloned()
                .collect(),
        };
        assert_benchmark_history(
            &second_benchmarks,
            &second.id,
            both.iter()
                .find(|report| report.instance_id == second.id.as_str())
                .unwrap(),
        );
        let repeated = service
            .import_instance(prepared(&source))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated.id, first.id);
        assert_eq!(reports(root.path()), both);
        assert_eq!(benchmark_history(root.path()), both_benchmarks);
        assert_eq!(snapshot(&source.baseline), before);
    }
}
