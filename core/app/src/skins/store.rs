//! Feature-owned SQLite schema and queries for the saved skin library.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::media::{
    SKIN_PNG_MAX_BYTES, SkinVariant, is_valid_normalized_skin_cache_png, texture_key,
};
use crate::storage::{MetadataStore, Migration, StorageError, rusqlite};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::library::{
    SavedSkinDeleteResult, SavedSkinRecord, SavedSkinSnapshot, SkinLibraryError, portable_token,
    validate_account_id, validate_cape_id, validate_name, validate_texture_key, validate_variant,
};

pub const MAX_SAVED_SKINS: usize = 32_768;
pub const MAX_TOTAL_PNG_BYTES: u64 = 512 * 1024 * 1024;

/// Registered after accounts::directory::MIGRATION. Account removal atomically
/// removes its applied marker in the same SQLite commit.
pub const MIGRATION: Migration = Migration {
    id: "skins.library.v1",
    sql: r#"
CREATE TABLE skin_library_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
);
INSERT INTO skin_library_state (singleton, revision) VALUES (1, 0);
CREATE TABLE saved_skins (
    texture_key TEXT PRIMARY KEY NOT NULL
        CHECK (length(texture_key) = 64 AND texture_key NOT GLOB '*[^0-9a-f]*'),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    variant TEXT NOT NULL CHECK (variant IN ('classic', 'slim')),
    source TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 64),
    cape_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    byte_size INTEGER NOT NULL CHECK (byte_size BETWEEN 1 AND 262144),
    png BLOB NOT NULL CHECK (length(png) = byte_size),
    revision INTEGER NOT NULL CHECK (revision > 0)
);
CREATE TABLE saved_skin_accounts (
    account_id TEXT PRIMARY KEY NOT NULL REFERENCES account_directory(account_id) ON DELETE CASCADE
        CHECK (length(account_id) BETWEEN 1 AND 128),
    texture_key TEXT NOT NULL REFERENCES saved_skins(texture_key) ON DELETE CASCADE,
    applied_at TEXT NOT NULL
);
CREATE INDEX saved_skin_accounts_texture ON saved_skin_accounts(texture_key);
"#,
};

#[derive(Clone)]
pub struct SavedSkinStore {
    metadata: Arc<MetadataStore>,
}

impl SavedSkinStore {
    pub fn new(metadata: Arc<MetadataStore>) -> Self {
        Self { metadata }
    }

    pub(super) fn list(
        &self,
        account_id: Option<&str>,
    ) -> Result<Vec<SavedSkinRecord>, SkinLibraryError> {
        self.metadata.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT s.texture_key, s.name, s.variant, s.source, s.cape_id,
                 s.created_at, s.updated_at,
                 CASE WHEN ?1 IS NULL THEN
                    (SELECT MAX(applied_at) FROM saved_skin_accounts a WHERE a.texture_key=s.texture_key)
                 ELSE (SELECT applied_at FROM saved_skin_accounts a
                       WHERE a.texture_key=s.texture_key AND a.account_id=?1) END,
                 s.byte_size, s.revision
                 FROM saved_skins s ORDER BY s.updated_at DESC, s.name, s.texture_key LIMIT 32769",
            )?;
            let rows = statement.query_map(params![account_id], read_snapshot)?;
            let mut records = Vec::new();
            let mut total = 0_u64;
            for row in rows {
                let snapshot = row?;
                validate_record(&snapshot.record)?;
                total = total.checked_add(snapshot.record.byte_size as u64)
                    .ok_or(SkinLibraryError::InvalidData)?;
                if records.len() == MAX_SAVED_SKINS || total > MAX_TOTAL_PNG_BYTES {
                    return Err(SkinLibraryError::InvalidData);
                }
                records.push(snapshot.record);
            }
            Ok(records)
        })
    }

    pub(super) fn get(&self, key: &str) -> Result<Option<SavedSkinSnapshot>, SkinLibraryError> {
        self.metadata.read(|connection| get(connection, key))
    }

    pub(super) fn read_png(&self, key: &str) -> Result<Option<Vec<u8>>, SkinLibraryError> {
        self.metadata.read(|connection| read_png(connection, key))
    }

    pub(super) fn save(
        &self,
        name: String,
        variant: SkinVariant,
        source: &str,
        cape_id: Option<String>,
        png: &[u8],
    ) -> Result<SavedSkinRecord, SkinLibraryError> {
        let key = texture_key(png);
        validate_png(&key, png)?;
        self.metadata.transaction(|transaction| {
            let previous = get(transaction, &key)?;
            if previous.is_some() {
                require_identical_png(transaction, &key, png)?;
            }
            let unchanged_profile = previous.as_ref().is_some_and(|previous| {
                previous.record.variant == variant && previous.record.cape_id == cape_id
            });
            if !unchanged_profile {
                clear_texture_accounts(transaction, &key)?;
            }
            let now = Utc::now().to_rfc3339();
            let record = SavedSkinRecord {
                texture_key: key.clone(),
                name,
                variant,
                source: source.to_owned(),
                cape_id,
                created_at: previous
                    .map(|previous| previous.record.created_at)
                    .unwrap_or_else(|| now.clone()),
                updated_at: now,
                applied_at: None,
                byte_size: png.len(),
            };
            write_record(transaction, &record, png)?;
            enforce_capacity(transaction)?;
            Ok(require_record(transaction, &key)?.record)
        })
    }

    pub(super) fn update_metadata(
        &self,
        key: &str,
        name: Option<String>,
        variant: Option<SkinVariant>,
        cape_id: Option<Option<String>>,
    ) -> Result<Option<SavedSkinRecord>, SkinLibraryError> {
        self.metadata.transaction(|transaction| {
            let Some(mut snapshot) = get(transaction, key)? else { return Ok(None) };
            let changed_profile = variant.is_some_and(|variant| variant != snapshot.record.variant)
                || cape_id.as_ref().is_some_and(|cape_id| cape_id != &snapshot.record.cape_id);
            if let Some(name) = name { snapshot.record.name = name; }
            if let Some(variant) = variant { snapshot.record.variant = variant; }
            if let Some(cape_id) = cape_id { snapshot.record.cape_id = cape_id; }
            if changed_profile {
                clear_texture_accounts(transaction, key)?;
            }
            snapshot.record.updated_at = Utc::now().to_rfc3339();
            validate_record(&snapshot.record)?;
            let revision = next_revision(transaction)?;
            transaction.execute(
                "UPDATE saved_skins SET name=?2, variant=?3, cape_id=?4, updated_at=?5, revision=?6 WHERE texture_key=?1",
                params![key, snapshot.record.name, snapshot.record.variant.as_str(), snapshot.record.cape_id,
                    snapshot.record.updated_at, revision],
            )?;
            Ok(Some(require_record(transaction, key)?.record))
        })
    }

    pub(super) fn replace_texture(
        &self,
        old_key: &str,
        name: Option<String>,
        variant: SkinVariant,
        cape_id: Option<Option<String>>,
        png: &[u8],
    ) -> Result<Option<SavedSkinSnapshot>, SkinLibraryError> {
        let new_key = texture_key(png);
        validate_png(&new_key, png)?;
        self.metadata.transaction(|transaction| {
            let Some(old) = get(transaction, old_key)? else {
                return Ok(None);
            };
            let existing = get(transaction, &new_key)?;
            if existing.is_some() {
                require_identical_png(transaction, &new_key, png)?;
            }
            let cape_id = cape_id.unwrap_or_else(|| old.record.cape_id.clone());
            let compatible_existing = existing.as_ref().is_some_and(|existing| {
                existing.record.variant == variant && existing.record.cape_id == cape_id
            });
            if !compatible_existing {
                clear_texture_accounts(transaction, &new_key)?;
            }
            let record = SavedSkinRecord {
                texture_key: new_key.clone(),
                name: name.unwrap_or(old.record.name),
                variant,
                source: old.record.source,
                cape_id,
                created_at: old.record.created_at,
                updated_at: Utc::now().to_rfc3339(),
                applied_at: None,
                byte_size: png.len(),
            };
            write_record(transaction, &record, png)?;
            if old_key != new_key {
                // A different texture does not change what was remotely applied.
                // Pending intents are retargeted by the library's post-commit observer.
                clear_texture_accounts(transaction, old_key)?;
                transaction.execute("DELETE FROM saved_skins WHERE texture_key=?1", [old_key])?;
            }
            enforce_capacity(transaction)?;
            Ok(Some(require_record(transaction, &new_key)?))
        })
    }

    pub(super) fn delete_unapplied(
        &self,
        key: &str,
    ) -> Result<SavedSkinDeleteResult, SkinLibraryError> {
        self.metadata.transaction(|transaction| {
            let Some(snapshot) = get(transaction, key)? else {
                return Ok(SavedSkinDeleteResult::Missing);
            };
            let applied: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM saved_skin_accounts WHERE texture_key=?1)",
                [key],
                |row| row.get(0),
            )?;
            if applied {
                return Ok(SavedSkinDeleteResult::Applied);
            }
            transaction.execute("DELETE FROM saved_skins WHERE texture_key=?1", [key])?;
            next_revision(transaction)?;
            Ok(SavedSkinDeleteResult::Deleted(snapshot.record))
        })
    }

    pub(super) fn mark_applied(
        &self,
        account_id: &str,
        key: &str,
        expected_revision: Option<u64>,
    ) -> Result<bool, SkinLibraryError> {
        validate_account_id(account_id)?;
        self.metadata.transaction(|transaction| {
            let Some(snapshot) = get(transaction, key)? else { return Ok(false) };
            if expected_revision.is_some_and(|revision| revision != snapshot.revision) { return Ok(false); }
            transaction.execute(
                "INSERT INTO saved_skin_accounts(account_id,texture_key,applied_at) VALUES (?1,?2,?3)
                 ON CONFLICT(account_id) DO UPDATE SET texture_key=excluded.texture_key,applied_at=excluded.applied_at",
                params![account_id, key, Utc::now().to_rfc3339()],
            )?;
            Ok(true)
        })
    }

    pub(super) fn clear_applied(&self, account_id: &str) -> Result<(), SkinLibraryError> {
        self.metadata.transaction(|transaction| {
            transaction.execute(
                "DELETE FROM saved_skin_accounts WHERE account_id=?1",
                [account_id],
            )?;
            Ok(())
        })
    }
}

fn get(connection: &Connection, key: &str) -> Result<Option<SavedSkinSnapshot>, SkinLibraryError> {
    let snapshot = connection
        .query_row(
            "SELECT s.texture_key,s.name,s.variant,s.source,s.cape_id,s.created_at,s.updated_at,
         (SELECT MAX(applied_at) FROM saved_skin_accounts a WHERE a.texture_key=s.texture_key),
         s.byte_size,s.revision
         FROM saved_skins s WHERE texture_key=?1",
            [key],
            read_snapshot,
        )
        .optional()?;
    if let Some(snapshot) = &snapshot {
        validate_record(&snapshot.record)?;
    }
    Ok(snapshot)
}

fn require_record(
    connection: &Connection,
    key: &str,
) -> Result<SavedSkinSnapshot, SkinLibraryError> {
    get(connection, key)?.ok_or(SkinLibraryError::InvalidData)
}

fn read_snapshot(row: &Row<'_>) -> rusqlite::Result<SavedSkinSnapshot> {
    let variant: String = row.get(2)?;
    let variant = match variant.as_str() {
        "classic" => SkinVariant::Classic,
        "slim" => SkinVariant::Slim,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let size: i64 = row.get(8)?;
    let revision: i64 = row.get(9)?;
    if size <= 0 || size > SKIN_PNG_MAX_BYTES as i64 || revision <= 0 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(SavedSkinSnapshot {
        record: SavedSkinRecord {
            texture_key: row.get(0)?,
            name: row.get(1)?,
            variant,
            source: row.get(3)?,
            cape_id: row.get(4)?,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
            applied_at: row.get(7)?,
            byte_size: size as usize,
        },
        revision: revision as u64,
    })
}

fn read_png(connection: &Connection, key: &str) -> Result<Option<Vec<u8>>, SkinLibraryError> {
    let Some(snapshot) = get(connection, key)? else {
        return Ok(None);
    };
    // Check SQLite's actual value length before copying the BLOB into Rust.
    let length: i64 = connection.query_row(
        "SELECT length(png) FROM saved_skins WHERE texture_key=?1",
        [key],
        |row| row.get(0),
    )?;
    if length != snapshot.record.byte_size as i64 || length > SKIN_PNG_MAX_BYTES as i64 {
        return Err(SkinLibraryError::InvalidData);
    }
    let png: Vec<u8> = connection.query_row(
        "SELECT png FROM saved_skins WHERE texture_key=?1",
        [key],
        |row| row.get(0),
    )?;
    validate_png(key, &png)?;
    Ok(Some(png))
}

fn require_identical_png(
    connection: &Connection,
    key: &str,
    png: &[u8],
) -> Result<(), SkinLibraryError> {
    if read_png(connection, key)?.as_deref() == Some(png) {
        Ok(())
    } else {
        Err(SkinLibraryError::InvalidData)
    }
}

fn write_record(
    transaction: &Transaction<'_>,
    record: &SavedSkinRecord,
    png: &[u8],
) -> Result<(), SkinLibraryError> {
    validate_record(record)?;
    let revision = next_revision(transaction)?;
    transaction.execute(
        "INSERT INTO saved_skins(texture_key,name,variant,source,cape_id,created_at,updated_at,byte_size,png,revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(texture_key) DO UPDATE SET name=excluded.name,variant=excluded.variant,
         source=excluded.source,cape_id=excluded.cape_id,created_at=excluded.created_at,
         updated_at=excluded.updated_at,
         byte_size=excluded.byte_size,png=excluded.png,revision=excluded.revision",
        params![record.texture_key,record.name,record.variant.as_str(),record.source,record.cape_id,
            record.created_at,record.updated_at,record.byte_size as i64,png,revision],
    )?;
    Ok(())
}

fn next_revision(transaction: &Transaction<'_>) -> Result<i64, SkinLibraryError> {
    let previous: i64 = transaction.query_row(
        "SELECT revision FROM skin_library_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let next = previous
        .checked_add(1)
        .filter(|revision| *revision > 0)
        .ok_or(SkinLibraryError::InvalidData)?;
    transaction.execute(
        "UPDATE skin_library_state SET revision=?1 WHERE singleton=1",
        [next],
    )?;
    Ok(next)
}

fn clear_texture_accounts(
    transaction: &Transaction<'_>,
    key: &str,
) -> Result<(), SkinLibraryError> {
    transaction.execute(
        "DELETE FROM saved_skin_accounts WHERE texture_key=?1",
        [key],
    )?;
    Ok(())
}

fn enforce_capacity(transaction: &Transaction<'_>) -> Result<(), SkinLibraryError> {
    let (count, total): (i64, i64) = transaction.query_row(
        "SELECT COUNT(*),COALESCE(SUM(byte_size),0) FROM saved_skins",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count > MAX_SAVED_SKINS as i64 || total > MAX_TOTAL_PNG_BYTES as i64 {
        return Err(SkinLibraryError::Capacity);
    }
    Ok(())
}

fn validate_png(key: &str, png: &[u8]) -> Result<(), SkinLibraryError> {
    if png.is_empty()
        || png.len() > SKIN_PNG_MAX_BYTES
        || texture_key(png) != key
        || !is_valid_normalized_skin_cache_png(png)
    {
        return Err(SkinLibraryError::InvalidData);
    }
    Ok(())
}

fn validate_record(record: &SavedSkinRecord) -> Result<(), SkinLibraryError> {
    let valid = validate_texture_key(&record.texture_key)
        .is_ok_and(|value| value == record.texture_key)
        && validate_name(&record.name).is_ok_and(|value| value == record.name)
        && validate_variant(record.variant.as_str()).is_ok()
        && !record.source.is_empty()
        && record.source.len() <= 64
        && portable_token(&record.source)
        && record
            .cape_id
            .as_ref()
            .is_none_or(|id| validate_cape_id(id).is_ok_and(|value| value == *id))
        && record.byte_size > 0
        && record.byte_size <= SKIN_PNG_MAX_BYTES;
    if !valid {
        return Err(SkinLibraryError::InvalidData);
    }
    for timestamp in [
        Some(&record.created_at),
        Some(&record.updated_at),
        record.applied_at.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if timestamp.len() > 64 || DateTime::parse_from_rfc3339(timestamp).is_err() {
            return Err(SkinLibraryError::InvalidData);
        }
    }
    Ok(())
}

impl From<rusqlite::Error> for SkinLibraryError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(error, _) => match error.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    Self::Conflict
                }
                rusqlite::ErrorCode::DiskFull => Self::StorageFull,
                rusqlite::ErrorCode::PermissionDenied | rusqlite::ErrorCode::ReadOnly => {
                    Self::PermissionDenied
                }
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                    Self::InvalidData
                }
                _ => Self::Storage,
            },
            rusqlite::Error::InvalidQuery
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::FromSqlConversionFailure(..) => Self::InvalidData,
            _ => Self::Storage,
        }
    }
}

impl From<StorageError> for SkinLibraryError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Corrupt => Self::InvalidData,
            StorageError::Busy | StorageError::Closed | StorageError::BudgetExceeded => {
                Self::Conflict
            }
            StorageError::Sqlite(error) => Self::from(error),
            _ => Self::Storage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{directory::AccountDirectory, model::offline_uuid};

    fn alice() -> String {
        format!("offline-{}", offline_uuid("Alice"))
    }
    fn bob() -> String {
        format!("offline-{}", offline_uuid("Bob"))
    }

    fn accounts(metadata: Arc<MetadataStore>) {
        let accounts = AccountDirectory::new(metadata).unwrap();
        accounts.create_offline_account("Alice").unwrap();
        accounts.create_offline_account("Bob").unwrap();
    }

    fn png(red: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[red, 20, 30, 255].repeat(64 * 64))
                .unwrap();
        }
        crate::media::normalize_skin_png(&bytes).unwrap().png_bytes
    }

    fn store() -> (Arc<MetadataStore>, SavedSkinStore) {
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        accounts(metadata.clone());
        metadata.migrate(&[MIGRATION]).unwrap();
        (metadata.clone(), SavedSkinStore::new(metadata))
    }

    fn save(store: &SavedSkinStore, bytes: &[u8], name: &str) -> SavedSkinRecord {
        store
            .save(
                name.into(),
                SkinVariant::Classic,
                "local_upload",
                None,
                bytes,
            )
            .unwrap()
    }

    #[test]
    fn png_and_metadata_survive_reopen_with_exact_content_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("skin-fixture.sqlite");
        let bytes = png(11);
        let key;
        let applied_at;
        {
            let metadata = Arc::new(MetadataStore::open(&path).unwrap());
            accounts(metadata.clone());
            metadata.migrate(&[MIGRATION]).unwrap();
            let store = SavedSkinStore::new(metadata);
            key = save(&store, &bytes, "A retained skin").texture_key;
            save(&store, &bytes, "Renamed without a duplicate");
            store.mark_applied(&alice(), &key, None).unwrap();
            applied_at = store.get(&key).unwrap().unwrap().record.applied_at;
            assert!(applied_at.is_some());
        }
        let metadata = Arc::new(MetadataStore::open(&path).unwrap());
        metadata.migrate(&[MIGRATION]).unwrap();
        let reopened = SavedSkinStore::new(metadata);
        let records = reopened.list(Some(&alice())).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "Renamed without a duplicate");
        assert_eq!(records[0].applied_at, applied_at);
        assert_eq!(reopened.list(None).unwrap()[0].applied_at, applied_at);
        assert_eq!(
            reopened.get(&key).unwrap().unwrap().record.applied_at,
            applied_at
        );
        assert_eq!(reopened.read_png(&key).unwrap().unwrap(), bytes);
    }

    #[test]
    fn applied_markers_are_account_bound_and_revision_fenced() {
        let (_, store) = store();
        let first = save(&store, &png(11), "First");
        let other = save(&store, &png(22), "Other");
        let old_revision = store.get(&first.texture_key).unwrap().unwrap().revision;
        store
            .mark_applied(&alice(), &first.texture_key, Some(old_revision))
            .unwrap();
        store
            .mark_applied(&bob(), &other.texture_key, None)
            .unwrap();
        let applied_at = store
            .get(&first.texture_key)
            .unwrap()
            .unwrap()
            .record
            .applied_at;
        let edited = store
            .update_metadata(&first.texture_key, Some("Edited".into()), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(edited.applied_at, applied_at);
        assert!(
            !store
                .mark_applied(&alice(), &first.texture_key, Some(old_revision))
                .unwrap()
        );
        assert!(matches!(
            store.delete_unapplied(&first.texture_key).unwrap(),
            SavedSkinDeleteResult::Applied
        ));
        store.clear_applied(&alice()).unwrap();
        assert!(matches!(
            store.delete_unapplied(&first.texture_key).unwrap(),
            SavedSkinDeleteResult::Deleted(_)
        ));
        assert!(store.list(Some(&bob())).unwrap()[0].applied_at.is_some());
        assert!(store.list(Some(&alice())).unwrap()[0].applied_at.is_none());
    }

    #[test]
    fn failed_replacement_rolls_back_bytes_records_and_account_markers() {
        let (metadata, store) = store();
        let bytes = png(11);
        let original = save(&store, &bytes, "Original");
        store
            .mark_applied(&alice(), &original.texture_key, None)
            .unwrap();
        metadata.transaction(|transaction| -> Result<(), StorageError> {
            transaction.execute_batch("CREATE TRIGGER reject_skin_replacement BEFORE DELETE ON saved_skins BEGIN SELECT RAISE(ABORT, 'injected replacement failure'); END;")?;
            Ok(())
        }).unwrap();
        assert!(
            store
                .replace_texture(
                    &original.texture_key,
                    None,
                    SkinVariant::Slim,
                    None,
                    &png(22)
                )
                .is_err()
        );
        let records = store.list(Some(&alice())).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].texture_key, original.texture_key);
        assert!(records[0].applied_at.is_some());
        assert_eq!(
            store.read_png(&original.texture_key).unwrap().unwrap(),
            bytes
        );
    }

    #[test]
    fn corrupt_or_wrong_content_address_is_never_delivered() {
        let (metadata, store) = store();
        let first = save(&store, &png(11), "Original");
        let wrong = png(22);
        metadata
            .transaction(|transaction| -> Result<(), StorageError> {
                transaction.execute(
                    "UPDATE saved_skins SET png=?1,byte_size=?2 WHERE texture_key=?3",
                    params![wrong, wrong.len() as i64, first.texture_key],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.read_png(&first.texture_key),
            Err(SkinLibraryError::InvalidData)
        );
    }

    #[test]
    fn account_removal_cascades_its_marker_without_clearing_another_account() {
        let (metadata, store) = store();
        let skin = save(&store, &png(11), "Shared texture");
        store
            .mark_applied(&alice(), &skin.texture_key, None)
            .unwrap();
        store.mark_applied(&bob(), &skin.texture_key, None).unwrap();
        let directory = AccountDirectory::new(metadata).unwrap();
        directory.remove(&alice()).unwrap();
        assert!(store.list(Some(&alice())).unwrap()[0].applied_at.is_none());
        assert!(store.list(Some(&bob())).unwrap()[0].applied_at.is_some());
        assert!(matches!(
            store.delete_unapplied(&skin.texture_key).unwrap(),
            SavedSkinDeleteResult::Applied
        ));
        directory.remove(&bob()).unwrap();
        assert!(matches!(
            store.delete_unapplied(&skin.texture_key).unwrap(),
            SavedSkinDeleteResult::Deleted(_)
        ));
    }
}
