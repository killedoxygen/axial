//! The registry owns instance rows; other features compose its commands inside
//! their own metadata transaction instead of writing the table directly.

use super::model::{
    Instance, InstanceError, InstanceId, InstanceLifecycle, InstancePatch, InstanceRecord,
    InstanceResult, validate_name,
};
use crate::storage::{
    MetadataStore, Migration,
    rusqlite::{self, OptionalExtension, Transaction, params},
};
use crate::{
    files::{PortableName, ScopedDirectory},
    library::{GenerationPin, LibraryLifecycle},
    tasks::{ExclusionLease, Exclusions},
};
use std::sync::Arc;

/// Sole issuer of registered instance file authority. Mutations reserve the
/// opaque target; reads retain and recheck its exact binding.
#[derive(Clone)]
pub struct InstanceDirectories {
    registry: Registry,
    library: LibraryLifecycle,
    exclusions: Exclusions,
}

impl InstanceDirectories {
    pub fn new(registry: Registry, library: LibraryLifecycle, exclusions: Exclusions) -> Self {
        Self {
            registry,
            library,
            exclusions,
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn library(&self) -> &LibraryLifecycle {
        &self.library
    }
    pub fn exclusions(&self) -> &Exclusions {
        &self.exclusions
    }

    pub fn admit(&self, id: &InstanceId) -> InstanceResult<RegisteredInstance> {
        self.admit_checked(id, true, true, true)
    }

    pub(crate) fn admit_read(&self, id: &InstanceId) -> InstanceResult<ReadInstance> {
        let pin = self
            .library
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let record = self.registry.get_live(id)?;
        let directory = open_registered_directory(&record, &pin)?;
        let admitted = ReadInstance {
            registry: self.registry.clone(),
            record,
            directory,
        };
        admitted.validate_current()?;
        Ok(admitted)
    }

    /// The session lends its existing exclusion for metadata only. Startup
    /// recency and earlier edits may have advanced the row since launch.
    pub(super) fn update_retained(
        &self,
        admitted: &RegisteredInstance,
        patch: InstancePatch,
    ) -> InstanceResult<InstanceRecord> {
        if !Arc::ptr_eq(&self.registry.storage, &admitted.registry.storage)
            || !admitted.exclusion.belongs_to(&self.exclusions)
        {
            return Err(InstanceError::Conflict);
        }
        let current = validate_live_binding(&self.registry, &admitted.record, &admitted.directory)?;
        self.registry
            .update(&current.instance.id, current.revision, patch)
    }

    pub(crate) fn admit_content_settlement(
        &self,
        id: &InstanceId,
    ) -> InstanceResult<RegisteredInstance> {
        self.admit_checked(id, false, true, false)
    }

    pub(crate) fn admit_for_performance_settlement(
        &self,
        id: &InstanceId,
    ) -> InstanceResult<RegisteredInstance> {
        self.admit_checked(id, true, false, true)
    }

    pub(crate) fn admit_for_setup_settlement(
        &self,
        id: &InstanceId,
    ) -> InstanceResult<RegisteredInstance> {
        self.admit_checked(id, true, true, false)
    }

    fn admit_checked(
        &self,
        id: &InstanceId,
        check_content: bool,
        check_performance: bool,
        check_setup: bool,
    ) -> InstanceResult<RegisteredInstance> {
        let pin = self
            .library
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let exclusion = self
            .exclusions
            .try_acquire([id.as_str()], [])
            .map_err(|_| InstanceError::Busy)?;
        if check_content && crate::content::install::has_pending(self.registry.storage(), id)? {
            return Err(InstanceError::Busy);
        }
        if check_performance
            && crate::performance::mutation::has_pending(self.registry.storage(), id)?
        {
            return Err(InstanceError::Busy);
        }
        if check_setup && super::setup::has_pending(self.registry.storage(), id)? {
            return Err(InstanceError::Busy);
        }
        let record = self.registry.get_live(id)?;
        Self::admit_record(self.registry.clone(), record, pin, exclusion)
    }

    pub(crate) fn admit_record(
        registry: Registry,
        record: InstanceRecord,
        pin: GenerationPin,
        exclusion: ExclusionLease,
    ) -> InstanceResult<RegisteredInstance> {
        let directory = open_registered_directory(&record, &pin)?;
        Ok(RegisteredInstance {
            registry,
            record,
            directory,
            pin,
            exclusion,
        })
    }
}

fn open_registered_directory(
    record: &InstanceRecord,
    pin: &GenerationPin,
) -> InstanceResult<ScopedDirectory> {
    if record.library_id != pin.library_id().to_string() {
        return Err(InstanceError::LibraryUnavailable);
    }
    let parent = instances_parent(pin)?;
    let name = PortableName::new_exact(&record.directory_name)
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let directory = parent
        .open_directory(&name)
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    directory
        .verify_receipt(
            record
                .directory_receipt
                .as_deref()
                .ok_or(InstanceError::DirectoryUnavailable)?,
        )
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    Ok(directory)
}

/// Bounded resource reads retain the directory's generation pin, not a mutation
/// lease. Recheck the live binding before returning owned bytes or metadata.
pub(crate) struct ReadInstance {
    registry: Registry,
    record: InstanceRecord,
    directory: ScopedDirectory,
}

impl ReadInstance {
    pub(crate) fn record(&self) -> &InstanceRecord {
        &self.record
    }

    pub(crate) fn game_directory(&self) -> &ScopedDirectory {
        &self.directory
    }

    pub(crate) fn validate_current(&self) -> InstanceResult<()> {
        validate_live_binding(&self.registry, &self.record, &self.directory).map(|_| ())
    }
}

fn validate_live_binding(
    registry: &Registry,
    captured: &InstanceRecord,
    directory: &ScopedDirectory,
) -> InstanceResult<InstanceRecord> {
    directory
        .pin()
        .revalidate()
        .map_err(|_| InstanceError::LibraryUnavailable)?;
    directory
        .verify_receipt(
            captured
                .directory_receipt
                .as_deref()
                .ok_or(InstanceError::DirectoryUnavailable)?,
        )
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let id = &captured.instance.id;
    let current = registry.get_live(id)?;
    if current.library_id != captured.library_id
        || current.directory_name != captured.directory_name
        || current.directory_receipt != captured.directory_receipt
        || current.instance.version_id != captured.instance.version_id
        || current.instance.loader_key != captured.instance.loader_key
        || current.instance.minecraft_version != captured.instance.minecraft_version
    {
        return Err(InstanceError::Conflict);
    }
    if crate::content::install::has_pending(registry.storage(), id)?
        || crate::performance::mutation::has_pending(registry.storage(), id)?
        || super::setup::has_pending(registry.storage(), id)?
    {
        return Err(InstanceError::Busy);
    }
    Ok(current)
}

/// Retain this value through all work, including process output drainage and
/// persistence. A clone transfers the same admission, not a new operation.
#[derive(Clone)]
pub struct RegisteredInstance {
    registry: Registry,
    record: InstanceRecord,
    directory: ScopedDirectory,
    pin: GenerationPin,
    exclusion: ExclusionLease,
}

impl RegisteredInstance {
    pub fn record(&self) -> &InstanceRecord {
        &self.record
    }
    pub(crate) fn directory(&self) -> &ScopedDirectory {
        &self.directory
    }
    pub(crate) fn game_directory(&self) -> &ScopedDirectory {
        &self.directory
    }
    pub fn generation(&self) -> &GenerationPin {
        &self.pin
    }
    pub fn exclusion(&self) -> &ExclusionLease {
        &self.exclusion
    }
    pub fn lease(&self) -> &ExclusionLease {
        &self.exclusion
    }

    pub fn validate_current(&self) -> InstanceResult<()> {
        self.pin
            .revalidate()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        self.directory
            .verify_receipt(
                self.record
                    .directory_receipt
                    .as_deref()
                    .ok_or(InstanceError::DirectoryUnavailable)?,
            )
            .map_err(|_| InstanceError::DirectoryUnavailable)?;
        let current = self.registry.get_live(&self.record.instance.id)?;
        if current != self.record {
            return Err(InstanceError::Conflict);
        }
        Ok(())
    }

    pub fn revalidate(&self) -> InstanceResult<()> {
        self.validate_current()
    }

    /// The session calls this only after observing successful startup, while
    /// retaining the same admission that was validated before process spawn.
    pub(crate) fn record_successful_launch(&self, launched_at: &str) -> InstanceResult<()> {
        self.validate_current()?;
        self.registry
            .record_successful_launch(&self.record, launched_at)
    }
}

pub(crate) fn instances_parent(pin: &GenerationPin) -> InstanceResult<ScopedDirectory> {
    pin.files()
        .map_err(|_| InstanceError::LibraryUnavailable)?
        .open_directory(&PortableName::new_exact("instances").expect("fixed portable name"))
        .map_err(|_| InstanceError::DirectoryUnavailable)
}

pub const MIGRATION: Migration = Migration {
    id: "instances.v1",
    sql: "CREATE TABLE instances (
        id TEXT PRIMARY KEY NOT NULL,
        name TEXT NOT NULL UNIQUE,
        revision INTEGER NOT NULL CHECK(revision > 0),
        lifecycle TEXT NOT NULL CHECK(lifecycle IN ('reserved','live','deleting')),
        library_id TEXT NOT NULL,
        directory_name TEXT NOT NULL,
        directory_receipt TEXT,
        record_json TEXT NOT NULL,
        UNIQUE(library_id, directory_name)
    );
    CREATE TABLE instance_selection (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        instance_id TEXT
    );
    INSERT INTO instance_selection(singleton, instance_id) VALUES(1, NULL);",
};

#[derive(Clone)]
pub struct Registry {
    storage: Arc<MetadataStore>,
}

impl Registry {
    /// Migrations are run once by the composition owner before construction.
    pub fn new(storage: Arc<MetadataStore>) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &MetadataStore {
        &self.storage
    }

    pub fn list(&self) -> InstanceResult<Vec<InstanceRecord>> {
        self.storage.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT record_json FROM instances WHERE lifecycle = 'live' ORDER BY rowid",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|row| decode(&row?))
                .collect()
        })
    }

    pub fn pending(&self) -> InstanceResult<Vec<InstanceRecord>> {
        self.storage.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT record_json FROM instances WHERE lifecycle != 'live' ORDER BY rowid",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|row| decode(&row?))
                .collect()
        })
    }

    pub fn get_record(&self, id: &InstanceId) -> InstanceResult<InstanceRecord> {
        self.storage.read(|connection| get_in(connection, id))
    }

    pub fn get_live(&self, id: &InstanceId) -> InstanceResult<InstanceRecord> {
        let record = self.get_record(id)?;
        if record.lifecycle != InstanceLifecycle::Live {
            return Err(InstanceError::Busy);
        }
        Ok(record)
    }

    pub fn last_instance_id(&self) -> InstanceResult<Option<InstanceId>> {
        self.storage.read(|connection| {
            let raw: Option<String> = connection.query_row(
                "SELECT instance_id FROM instance_selection WHERE singleton = 1",
                [],
                |row| row.get(0),
            )?;
            raw.map(|id| id.parse()).transpose()
        })
    }

    /// First import publication may fill an empty selection without recording
    /// a new launch or replacing a destination choice.
    pub(super) fn restore_import_selection(
        &self,
        tx: &Transaction<'_>,
        expected: &InstanceRecord,
    ) -> InstanceResult<()> {
        let record = get_in(tx, &expected.instance.id)?;
        require_exact(&record, expected, InstanceLifecycle::Live)?;
        let selected: Option<String> = tx.query_row(
            "SELECT instance_id FROM instance_selection WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if let Some(selected) = selected {
            let _: InstanceId = selected.parse()?;
            return Ok(());
        }
        let changed = tx.execute(
            "UPDATE instance_selection SET instance_id = ?1 WHERE singleton = 1 AND instance_id IS NULL",
            [record.instance.id.as_str()],
        )?;
        if changed != 1 {
            return Err(InstanceError::Conflict);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn select(&self, id: &InstanceId) -> InstanceResult<()> {
        self.storage.transaction(|tx| {
            let record = get_in(tx, id)?;
            if record.lifecycle != InstanceLifecycle::Live {
                return Err(InstanceError::Busy);
            }
            tx.execute(
                "UPDATE instance_selection SET instance_id = ?1 WHERE singleton = 1",
                [id.as_str()],
            )?;
            Ok(())
        })
    }

    fn record_successful_launch(
        &self,
        expected: &InstanceRecord,
        launched_at: &str,
    ) -> InstanceResult<()> {
        self.storage.transaction(|tx| {
            let mut record = get_in(tx, &expected.instance.id)?;
            require_exact(&record, expected, InstanceLifecycle::Live)?;
            record.instance.last_played_at = launched_at.to_owned();
            advance(&mut record)?;
            write(tx, &record)?;
            let selected = tx.execute(
                "UPDATE instance_selection SET instance_id = ?1 WHERE singleton = 1",
                [record.instance.id.as_str()],
            )?;
            if selected != 1 {
                return Err(InstanceError::Conflict);
            }
            Ok(())
        })
    }

    /// Call only inside an admitted operation holding this instance's exclusion.
    pub fn update(
        &self,
        id: &InstanceId,
        expected_revision: u64,
        patch: InstancePatch,
    ) -> InstanceResult<InstanceRecord> {
        self.storage.transaction(|tx| {
            let mut record = get_in(tx, id)?;
            require(&record, expected_revision, InstanceLifecycle::Live)?;
            patch.apply(&mut record.instance)?;
            if name_taken(tx, &record.instance.name, Some(id))? {
                return Err(InstanceError::NameConflict);
            }
            advance(&mut record)?;
            write(tx, &record)?;
            Ok(record)
        })
    }

    /// Reserve the generated identity before creating any payload directory.
    /// An incomplete reservation is durable and never appears in live listings.
    pub fn reserve(
        &self,
        tx: &Transaction<'_>,
        instance: Instance,
        library_id: &str,
    ) -> InstanceResult<InstanceRecord> {
        let mut instance = instance;
        instance.name = validate_name(&instance.name)?;
        instance
            .settings
            .validate()
            .map_err(|_| InstanceError::InvalidSettings)?;
        if library_id.is_empty() || library_id.len() > 128 {
            return Err(InstanceError::InvalidInput);
        }
        if name_taken(tx, &instance.name, None)? {
            return Err(InstanceError::NameConflict);
        }
        instance.revision = 1;
        let record = InstanceRecord {
            directory_name: instance.id.as_str().to_owned(),
            instance,
            revision: 1,
            lifecycle: InstanceLifecycle::Reserved,
            library_id: library_id.to_owned(),
            directory_receipt: None,
        };
        tx.execute("INSERT INTO instances(id,name,revision,lifecycle,library_id,directory_name,directory_receipt,record_json)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", record_params(&record)?)?;
        Ok(record)
    }

    pub fn reserve_duplicate(
        &self,
        tx: &Transaction<'_>,
        source: &InstanceRecord,
        new_id: InstanceId,
        requested_name: Option<&str>,
        library_id: &str,
    ) -> InstanceResult<InstanceRecord> {
        let current = get_in(tx, &source.instance.id)?;
        require(&current, source.revision, InstanceLifecycle::Live)?;
        if current != *source {
            return Err(InstanceError::Conflict);
        }
        let mut names = tx.prepare("SELECT name FROM instances")?;
        let occupied = names
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let name = super::duplicate::choose_duplicate_name(
            &source.instance.name,
            &occupied,
            requested_name,
        )
        .map_err(|_| InstanceError::NameConflict)?;
        let mut instance = source.instance.clone();
        instance.id = new_id;
        instance.name = name;
        instance.created_at = chrono::Utc::now().to_rfc3339();
        instance.last_played_at.clear();
        instance.art_seed = new_art_seed(&instance.id);
        self.reserve(tx, instance, library_id)
    }

    /// Files must be verified by their owner before this registry command. A
    /// receipt string alone is deliberately insufficient to admit a directory.
    pub fn commit_reserved(
        &self,
        tx: &Transaction<'_>,
        expected: &InstanceRecord,
        receipt: &str,
    ) -> InstanceResult<InstanceRecord> {
        if receipt.is_empty() || receipt.len() > 4096 {
            return Err(InstanceError::InvalidInput);
        }
        let mut record = get_in(tx, &expected.instance.id)?;
        require_exact(&record, expected, InstanceLifecycle::Reserved)?;
        record.lifecycle = InstanceLifecycle::Live;
        record.directory_receipt = Some(receipt.to_owned());
        advance(&mut record)?;
        write(tx, &record)?;
        Ok(record)
    }

    pub fn abandon_reserved(
        &self,
        tx: &Transaction<'_>,
        expected: &InstanceRecord,
    ) -> InstanceResult<()> {
        let record = get_in(tx, &expected.instance.id)?;
        require_exact(&record, expected, InstanceLifecycle::Reserved)?;
        tx.execute(
            "DELETE FROM instances WHERE id = ?1",
            [record.instance.id.as_str()],
        )?;
        Ok(())
    }

    pub fn mark_deleting(
        &self,
        tx: &Transaction<'_>,
        id: &InstanceId,
        expected_revision: u64,
    ) -> InstanceResult<InstanceRecord> {
        let mut record = get_in(tx, id)?;
        require(&record, expected_revision, InstanceLifecycle::Live)?;
        record.lifecycle = InstanceLifecycle::Deleting;
        advance(&mut record)?;
        write(tx, &record)?;
        Ok(record)
    }

    pub fn restore_live(
        &self,
        tx: &Transaction<'_>,
        expected: &InstanceRecord,
    ) -> InstanceResult<()> {
        let mut record = get_in(tx, &expected.instance.id)?;
        require_exact(&record, expected, InstanceLifecycle::Deleting)?;
        record.lifecycle = InstanceLifecycle::Live;
        advance(&mut record)?;
        write(tx, &record)
    }

    pub fn remove(&self, tx: &Transaction<'_>, expected: &InstanceRecord) -> InstanceResult<()> {
        let record = get_in(tx, &expected.instance.id)?;
        require_exact(&record, expected, InstanceLifecycle::Deleting)?;
        tx.execute(
            "DELETE FROM instances WHERE id = ?1",
            [record.instance.id.as_str()],
        )?;
        tx.execute(
            "UPDATE instance_selection SET instance_id = NULL WHERE instance_id = ?1",
            [record.instance.id.as_str()],
        )?;
        Ok(())
    }
}

pub(crate) fn get_in(
    connection: &rusqlite::Connection,
    id: &InstanceId,
) -> InstanceResult<InstanceRecord> {
    let raw: Option<(String, String)> = connection
        .query_row(
            "SELECT lifecycle, record_json FROM instances WHERE id = ?1",
            [id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (lifecycle, raw) = raw.ok_or(InstanceError::NotFound)?;
    let record = decode(&raw)?;
    if record.instance.id != *id || record.lifecycle.as_str() != lifecycle {
        return Err(InstanceError::InvalidInput);
    }
    Ok(record)
}

pub(super) fn decode(raw: &str) -> InstanceResult<InstanceRecord> {
    let record: InstanceRecord =
        serde_json::from_str(raw).map_err(|_| InstanceError::InvalidInput)?;
    if record.revision == 0
        || record.revision != record.instance.revision
        || record.directory_name != record.instance.id.as_str()
    {
        return Err(InstanceError::InvalidInput);
    }
    validate_name(&record.instance.name)?;
    record
        .instance
        .settings
        .validate()
        .map_err(|_| InstanceError::InvalidSettings)?;
    Ok(record)
}

fn name_taken(
    connection: &rusqlite::Connection,
    name: &str,
    except: Option<&InstanceId>,
) -> InstanceResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM instances WHERE name = ?1 AND (?2 IS NULL OR id != ?2))",
        params![name, except.map(InstanceId::as_str)],
        |row| row.get(0),
    )?)
}

fn record_params(record: &InstanceRecord) -> InstanceResult<[rusqlite::types::Value; 8]> {
    use rusqlite::types::Value;
    let json = serde_json::to_string(record).map_err(|_| InstanceError::InvalidInput)?;
    let revision = i64::try_from(record.revision).map_err(|_| InstanceError::Conflict)?;
    Ok([
        record.instance.id.as_str().to_owned().into(),
        record.instance.name.clone().into(),
        revision.into(),
        record.lifecycle.as_str().to_owned().into(),
        record.library_id.clone().into(),
        record.directory_name.clone().into(),
        record
            .directory_receipt
            .clone()
            .map(Value::Text)
            .unwrap_or(Value::Null),
        json.into(),
    ])
}

fn write(tx: &Transaction<'_>, record: &InstanceRecord) -> InstanceResult<()> {
    let affected = tx.execute(
        "UPDATE instances SET name=?2,revision=?3,lifecycle=?4,library_id=?5,
        directory_name=?6,directory_receipt=?7,record_json=?8 WHERE id=?1",
        record_params(record)?,
    )?;
    if affected != 1 {
        return Err(InstanceError::NotFound);
    }
    Ok(())
}

fn require(record: &InstanceRecord, revision: u64, state: InstanceLifecycle) -> InstanceResult<()> {
    if record.revision != revision {
        return Err(InstanceError::Conflict);
    }
    if record.lifecycle != state {
        return Err(InstanceError::Busy);
    }
    Ok(())
}

fn require_exact(
    record: &InstanceRecord,
    expected: &InstanceRecord,
    state: InstanceLifecycle,
) -> InstanceResult<()> {
    require(record, expected.revision, state)?;
    if record != expected {
        return Err(InstanceError::Conflict);
    }
    Ok(())
}

fn advance(record: &mut InstanceRecord) -> InstanceResult<()> {
    record.revision = record
        .revision
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(InstanceError::Conflict)?;
    record.instance.revision = record.revision;
    Ok(())
}

pub(crate) fn new_art_seed(id: &InstanceId) -> u32 {
    let uuid = uuid::Uuid::parse_str(id.as_str()).expect("validated instance identity");
    u32::from_le_bytes(uuid.as_bytes()[..4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::InstanceSettings;

    fn instance(name: &str) -> Instance {
        Instance {
            id: InstanceId::new(),
            name: name.to_owned(),
            version_id: "1.21.1".to_owned(),
            created_at: "2026-09-08T09:00:00Z".to_owned(),
            last_played_at: String::new(),
            art_seed: 42,
            settings: InstanceSettings::default(),
            icon: String::new(),
            accent: String::new(),
            loader_key: "vanilla".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            revision: 0,
        }
    }

    fn memory() -> Registry {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage.migrate(&[MIGRATION]).unwrap();
        Registry::new(storage)
    }

    fn publish(registry: &Registry, name: &str) -> InstanceRecord {
        registry
            .storage()
            .transaction(|tx| {
                let reserved = registry.reserve(tx, instance(name), "test-library")?;
                registry.commit_reserved(tx, &reserved, "owner-verified-test-receipt")
            })
            .unwrap()
    }

    #[test]
    fn restart_preserves_identity_settings_selection_and_library() {
        let temp = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let path = temp.path().join("metadata.sqlite3");
        let expected = {
            let storage = Arc::new(MetadataStore::open(&path).unwrap());
            storage.migrate(&[MIGRATION]).unwrap();
            let registry = Registry::new(storage);
            let created = publish(&registry, "Weekend / friends");
            registry.select(&created.instance.id).unwrap();
            registry
                .update(
                    &created.instance.id,
                    created.revision,
                    InstancePatch {
                        name: Some("Renamed / display label".to_owned()),
                        max_memory_mb: Some(4096),
                        window_width: Some(1280),
                        ..InstancePatch::default()
                    },
                )
                .unwrap()
        };
        let storage = Arc::new(MetadataStore::open(&path).unwrap());
        storage.migrate(&[MIGRATION]).unwrap();
        let registry = Registry::new(storage);
        assert_eq!(registry.list().unwrap(), vec![expected.clone()]);
        assert_eq!(
            registry.last_instance_id().unwrap(),
            Some(expected.instance.id.clone())
        );
        assert_eq!(expected.directory_name, expected.instance.id.as_str());
        assert_eq!(expected.library_id, "test-library");
        assert_ne!(expected.directory_name, expected.instance.name);
    }

    #[test]
    fn reservation_is_not_live_and_reserves_its_name() {
        let registry = memory();
        let reserved = registry
            .storage()
            .transaction(|tx| registry.reserve(tx, instance("Survival"), "library"))
            .unwrap();
        assert!(registry.list().unwrap().is_empty());
        assert_eq!(registry.pending().unwrap(), vec![reserved.clone()]);
        assert!(matches!(
            registry.get_live(&reserved.instance.id),
            Err(InstanceError::Busy)
        ));
        assert!(matches!(
            registry.storage().transaction(|tx| registry.reserve(
                tx,
                instance("Survival"),
                "library"
            )),
            Err(InstanceError::NameConflict)
        ));
    }

    #[test]
    fn lookup_rejects_indexed_identity_or_lifecycle_disagreement() {
        for field in ["identity", "lifecycle"] {
            let registry = memory();
            let current = publish(&registry, "Survival");
            registry
                .storage()
                .transaction(|tx| -> InstanceResult<()> {
                    if field == "identity" {
                        let mut other = current.clone();
                        other.instance.id = InstanceId::new();
                        other.directory_name = other.instance.id.as_str().to_owned();
                        tx.execute(
                            "UPDATE instances SET record_json=?1 WHERE id=?2",
                            params![
                                serde_json::to_string(&other).unwrap(),
                                current.instance.id.as_str()
                            ],
                        )?;
                    } else {
                        tx.execute(
                            "UPDATE instances SET lifecycle='deleting' WHERE id=?1",
                            [current.instance.id.as_str()],
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(
                    registry.get_record(&current.instance.id),
                    Err(InstanceError::InvalidInput)
                ),
                "{field}"
            );
            assert!(
                matches!(
                    registry.get_live(&current.instance.id),
                    Err(InstanceError::InvalidInput)
                ),
                "{field}"
            );
        }
    }

    #[test]
    fn stale_edit_and_version_retarget_leave_stored_instance_unchanged() {
        let registry = memory();
        let current = publish(&registry, "Survival");
        assert!(matches!(
            registry.update(
                &current.instance.id,
                current.revision - 1,
                InstancePatch {
                    name: Some("Changed".to_owned()),
                    ..Default::default()
                }
            ),
            Err(InstanceError::Conflict)
        ));
        assert!(matches!(
            registry.update(
                &current.instance.id,
                current.revision,
                InstancePatch {
                    version_id: Some("different-version".to_owned()),
                    ..Default::default()
                }
            ),
            Err(InstanceError::Conflict)
        ));
        assert_eq!(registry.get_live(&current.instance.id).unwrap(), current);
    }

    #[test]
    fn launch_recency_rolls_back_both_fields_when_selection_write_fails() {
        let registry = memory();
        let previous = publish(&registry, "Previous");
        registry
            .record_successful_launch(&previous, "2026-09-26T10:00:00.000Z")
            .unwrap();
        let current = publish(&registry, "Current");
        registry
            .storage()
            .transaction(|tx| -> InstanceResult<()> {
                tx.execute_batch(
                    "CREATE TRIGGER reject_launch_selection BEFORE UPDATE ON instance_selection
                     BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            registry
                .record_successful_launch(&current, "2026-09-27T10:00:00.000Z")
                .is_err()
        );
        assert_eq!(registry.get_live(&current.instance.id).unwrap(), current);
        assert_eq!(
            registry.last_instance_id().unwrap(),
            Some(previous.instance.id)
        );
    }

    #[test]
    fn stale_launch_cannot_overwrite_instance_settings_or_selection() {
        let registry = memory();
        let captured = publish(&registry, "Current");
        let edited = registry
            .update(
                &captured.instance.id,
                captured.revision,
                InstancePatch {
                    max_memory_mb: Some(4096),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(matches!(
            registry.record_successful_launch(&captured, "2026-09-27T10:00:00.000Z"),
            Err(InstanceError::Conflict)
        ));
        assert_eq!(registry.get_live(&captured.instance.id).unwrap(), edited);
        assert!(registry.last_instance_id().unwrap().is_none());
    }

    #[test]
    fn deletion_transition_and_domain_error_are_atomic() {
        let registry = memory();
        let current = publish(&registry, "Survival");
        let result: InstanceResult<()> = registry.storage().transaction(|tx| {
            registry.mark_deleting(tx, &current.instance.id, current.revision)?;
            Err(InstanceError::Conflict)
        });
        assert!(matches!(result, Err(InstanceError::Conflict)));
        assert_eq!(registry.get_live(&current.instance.id).unwrap(), current);
        let deleting = registry
            .storage()
            .transaction(|tx| registry.mark_deleting(tx, &current.instance.id, current.revision))
            .unwrap();
        assert!(registry.list().unwrap().is_empty());
        assert!(matches!(
            registry.update(
                &current.instance.id,
                deleting.revision,
                InstancePatch::default()
            ),
            Err(InstanceError::Busy)
        ));
        registry
            .storage()
            .transaction(|tx| registry.restore_live(tx, &deleting))
            .unwrap();
        assert!(matches!(
            registry
                .storage()
                .transaction(|tx| registry.remove(tx, &deleting)),
            Err(InstanceError::Conflict)
        ));
        assert_eq!(registry.list().unwrap().len(), 1);
    }
}
