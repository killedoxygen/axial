//! Feature-owned SQLite schema and queries for the saved skin library.

use std::{collections::BTreeSet, sync::Arc};

use chrono::{DateTime, Utc};

use crate::media::{
    SKIN_PNG_MAX_BYTES, SkinVariant, is_valid_normalized_skin_cache_png, texture_key,
};
use crate::storage::{MetadataStore, Migration, StorageError, rusqlite};
use crate::tasks::CancellationToken;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::library::{
    PreparedSkinRecord, SavedSkinDeleteResult, SavedSkinRecord, SavedSkinSnapshot,
    SkinImportCommit, SkinImportReceipt, SkinLibraryError, portable_token, skin_import_id,
    validate_account_id, validate_cape_id, validate_name, validate_texture_key, validate_variant,
};

pub const MAX_SAVED_SKINS: usize = 32_768;
pub const MAX_TOTAL_PNG_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMPORT_KEYS_BYTES: usize = MAX_SAVED_SKINS * 67 + 2;

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
    imported_applied_at TEXT,
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

/// A completed fact committed with its PNGs, not a pending publication journal.
pub const IMPORT_MIGRATION: Migration = Migration {
    id: "skins.imports.v1",
    sql: r#"
CREATE TABLE saved_skin_imports (
    source_id TEXT PRIMARY KEY NOT NULL
        CHECK(length(source_id)=64 AND source_id NOT GLOB '*[^0-9a-f]*'),
    fingerprint TEXT NOT NULL
        CHECK(length(fingerprint)=64 AND fingerprint NOT GLOB '*[^0-9a-f]*'),
    import_id TEXT UNIQUE NOT NULL
        CHECK(length(import_id)=64 AND import_id NOT GLOB '*[^0-9a-f]*'),
    texture_keys TEXT NOT NULL
        CHECK(length(CAST(texture_keys AS BLOB)) BETWEEN 2 AND 2195458)
);
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
                 CASE WHEN ?1 IS NULL THEN COALESCE(
                    (SELECT MAX(applied_at) FROM saved_skin_accounts a WHERE a.texture_key=s.texture_key),
                    s.imported_applied_at)
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
            let imported_applied_at = if unchanged_profile {
                imported_marker(transaction, &key)?
            } else {
                clear_texture_accounts(transaction, &key)?;
                None
            };
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
                applied_at: imported_applied_at.clone(),
                byte_size: png.len(),
            };
            write_record(transaction, &record, png, imported_applied_at.as_deref())?;
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
                transaction.execute("UPDATE saved_skins SET imported_applied_at=NULL WHERE texture_key=?1", [key])?;
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
            let imported_applied_at = if compatible_existing {
                imported_marker(transaction, &new_key)?
            } else {
                clear_texture_accounts(transaction, &new_key)?;
                None
            };
            let record = SavedSkinRecord {
                texture_key: new_key.clone(),
                name: name.unwrap_or(old.record.name),
                variant,
                source: old.record.source,
                cape_id,
                created_at: old.record.created_at,
                updated_at: Utc::now().to_rfc3339(),
                applied_at: imported_applied_at.clone(),
                byte_size: png.len(),
            };
            write_record(transaction, &record, png, imported_applied_at.as_deref())?;
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
            // A legacy timestamp has no account identity and is historical
            // metadata. Only a live account reference protects local deletion.
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

    pub(super) fn import_batch(
        &self,
        source_id: &str,
        fingerprint: &str,
        records: &[PreparedSkinRecord],
        cancel: &CancellationToken,
    ) -> Result<SkinImportCommit, SkinLibraryError> {
        let import_id = skin_import_id(source_id, fingerprint)?;
        if records.len() > MAX_SAVED_SKINS {
            return Err(SkinLibraryError::Capacity);
        }
        let mut keys = BTreeSet::new();
        let mut bytes = 0_u64;
        for skin in records {
            if !keys.insert(skin.record.texture_key.clone()) {
                return Err(SkinLibraryError::InvalidData);
            }
            bytes = bytes
                .checked_add(skin.png.len() as u64)
                .ok_or(SkinLibraryError::Capacity)?;
            if bytes > MAX_TOTAL_PNG_BYTES {
                return Err(SkinLibraryError::Capacity);
            }
        }
        let receipt = SkinImportReceipt {
            skin_import_id: import_id,
            fingerprint: fingerprint.to_owned(),
            texture_keys: keys.into_iter().collect(),
        };
        let encoded = serde_json::to_string(&receipt.texture_keys)
            .map_err(|_| SkinLibraryError::InvalidData)?;
        self.metadata.transaction(|transaction| {
            let existing: Option<String> = transaction.query_row(
                "SELECT import_id FROM saved_skin_imports WHERE source_id=?1", [source_id], |row| row.get(0)
            ).optional()?;
            if let Some(existing) = existing {
                if existing != receipt.skin_import_id { return Err(SkinLibraryError::Conflict); }
                let completed = read_import_receipt(transaction, &existing)?.ok_or(SkinLibraryError::InvalidData)?;
                if completed != receipt { return Err(SkinLibraryError::Conflict); }
                return Ok(SkinImportCommit { receipt: completed, already_imported: true });
            }
            check_import_cancelled(cancel)?;
            for skin in records {
                check_import_cancelled(cancel)?;
                import_record_in_transaction(transaction, skin)?;
            }
            enforce_capacity(transaction)?;
            transaction.execute(
                "INSERT INTO saved_skin_imports(source_id,fingerprint,import_id,texture_keys) VALUES(?1,?2,?3,?4)",
                params![source_id, fingerprint, receipt.skin_import_id, encoded],
            )?;
            check_import_cancelled(cancel)?;
            Ok(SkinImportCommit { receipt, already_imported: false })
        })
    }

    pub(super) fn import_status(
        &self,
        import_id: &str,
    ) -> Result<Option<SkinImportReceipt>, SkinLibraryError> {
        if validate_texture_key(import_id).is_err() || import_id.trim() != import_id {
            return Err(SkinLibraryError::InvalidData);
        }
        self.metadata
            .read(|connection| read_import_receipt(connection, import_id))
    }

    pub(super) fn clear_imported_applied(&self, key: &str) -> Result<(), SkinLibraryError> {
        self.metadata.transaction(|transaction| {
            transaction.execute(
                "UPDATE saved_skins SET imported_applied_at=NULL WHERE texture_key=?1",
                [key],
            )?;
            Ok(())
        })
    }
}

fn check_import_cancelled(cancel: &CancellationToken) -> Result<(), SkinLibraryError> {
    if cancel.is_cancelled() {
        Err(SkinLibraryError::Conflict)
    } else {
        Ok(())
    }
}

pub(super) fn validate_import_record(
    record: &SavedSkinRecord,
    png: &[u8],
) -> Result<(), SkinLibraryError> {
    validate_record(record)?;
    validate_png(&record.texture_key, png)?;
    if record.byte_size != png.len() {
        return Err(SkinLibraryError::InvalidData);
    }
    Ok(())
}

fn import_record_in_transaction(
    transaction: &Transaction<'_>,
    skin: &PreparedSkinRecord,
) -> Result<(), SkinLibraryError> {
    let record = &skin.record;
    if let Some(mut existing) = get(transaction, &record.texture_key)? {
        // Compare retained history separately from live account-applied markers.
        existing.record.applied_at = imported_marker(transaction, &record.texture_key)?;
        let same_png: bool = transaction.query_row(
            "SELECT png=?2 FROM saved_skins WHERE texture_key=?1",
            params![record.texture_key, skin.png.as_ref()],
            |row| row.get(0),
        )?;
        if existing.record != *record || !same_png {
            return Err(SkinLibraryError::Conflict);
        }
        return Ok(());
    }
    write_record(transaction, record, &skin.png, record.applied_at.as_deref())
}

fn read_import_receipt(
    connection: &Connection,
    import_id: &str,
) -> Result<Option<SkinImportReceipt>, SkinLibraryError> {
    let row = connection
        .query_row(
            "SELECT source_id,fingerprint,length(CAST(texture_keys AS BLOB)),
         CASE WHEN length(CAST(texture_keys AS BLOB))<=?2 THEN texture_keys END
         FROM saved_skin_imports WHERE import_id=?1",
            params![import_id, MAX_IMPORT_KEYS_BYTES as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, usize>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((source_id, fingerprint, size, encoded)) = row else {
        return Ok(None);
    };
    if size > MAX_IMPORT_KEYS_BYTES || skin_import_id(&source_id, &fingerprint)? != import_id {
        return Err(SkinLibraryError::InvalidData);
    }
    let keys: Vec<String> = serde_json::from_str(&encoded.ok_or(SkinLibraryError::InvalidData)?)
        .map_err(|_| SkinLibraryError::InvalidData)?;
    if keys.len() > MAX_SAVED_SKINS
        || keys
            .iter()
            .any(|key| validate_texture_key(key).is_err() || key.trim() != key)
        || keys.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(SkinLibraryError::InvalidData);
    }
    Ok(Some(SkinImportReceipt {
        skin_import_id: import_id.to_owned(),
        fingerprint,
        texture_keys: keys,
    }))
}

fn get(connection: &Connection, key: &str) -> Result<Option<SavedSkinSnapshot>, SkinLibraryError> {
    let snapshot = connection.query_row(
        "SELECT s.texture_key,s.name,s.variant,s.source,s.cape_id,s.created_at,s.updated_at,
         COALESCE((SELECT MAX(applied_at) FROM saved_skin_accounts a WHERE a.texture_key=s.texture_key),
                  s.imported_applied_at),s.byte_size,s.revision
         FROM saved_skins s WHERE texture_key=?1",
        [key], read_snapshot,
    ).optional()?;
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

fn imported_marker(connection: &Connection, key: &str) -> Result<Option<String>, SkinLibraryError> {
    Ok(connection
        .query_row(
            "SELECT imported_applied_at FROM saved_skins WHERE texture_key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

fn write_record(
    transaction: &Transaction<'_>,
    record: &SavedSkinRecord,
    png: &[u8],
    imported_applied_at: Option<&str>,
) -> Result<(), SkinLibraryError> {
    validate_record(record)?;
    let revision = next_revision(transaction)?;
    transaction.execute(
        "INSERT INTO saved_skins(texture_key,name,variant,source,cape_id,created_at,updated_at,imported_applied_at,byte_size,png,revision)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(texture_key) DO UPDATE SET name=excluded.name,variant=excluded.variant,
         source=excluded.source,cape_id=excluded.cape_id,created_at=excluded.created_at,
         updated_at=excluded.updated_at,imported_applied_at=excluded.imported_applied_at,
         byte_size=excluded.byte_size,png=excluded.png,revision=excluded.revision",
        params![record.texture_key,record.name,record.variant.as_str(),record.source,record.cape_id,
            record.created_at,record.updated_at,imported_applied_at,record.byte_size as i64,png,revision],
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
    use crate::skins::library::prepare_import_record;

    const SOURCE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const FINGERPRINT: &str = "2222222222222222222222222222222222222222222222222222222222222222";

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
        metadata.migrate(&[MIGRATION, IMPORT_MIGRATION]).unwrap();
        (metadata.clone(), SavedSkinStore::new(metadata))
    }

    fn imported(red: u8, name: &str) -> PreparedSkinRecord {
        let bytes = png(red);
        prepare_import_record(
            SavedSkinRecord {
                texture_key: texture_key(&bytes),
                name: name.into(),
                variant: SkinVariant::Slim,
                source: "minecraft_profile_skin".into(),
                cape_id: Some("retained-cape".into()),
                created_at: "2024-01-01T00:00:00Z".into(),
                updated_at: "2024-01-02T00:00:00Z".into(),
                applied_at: Some("2024-01-03T00:00:00Z".into()),
                byte_size: bytes.len(),
            },
            bytes,
        )
        .unwrap()
    }

    fn revision(metadata: &MetadataStore) -> i64 {
        metadata
            .read(|connection| -> Result<_, StorageError> {
                Ok(
                    connection.query_row("SELECT revision FROM skin_library_state", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap()
    }

    #[test]
    fn skin_import_preparation_requires_exact_metadata_and_normalized_bytes() {
        let valid = imported(11, "Retained skin");
        for field in ["name", "source", "timestamp", "size", "key"] {
            let mut record = valid.record.clone();
            match field {
                "name" => record.name = " silently trimmed ".into(),
                "source" => record.source = "../remote".into(),
                "timestamp" => record.applied_at = Some("yesterday".into()),
                "size" => record.byte_size += 1,
                _ => record.texture_key = SOURCE.into(),
            }
            assert!(
                prepare_import_record(record, valid.png.to_vec()).is_err(),
                "{field}"
            );
        }
        let invalid_png = b"not a PNG".to_vec();
        let mut record = valid.record.clone();
        record.texture_key = texture_key(&invalid_png);
        record.byte_size = invalid_png.len();
        assert!(prepare_import_record(record, invalid_png).is_err());
    }

    #[test]
    fn skin_import_survives_restart_and_replay_preserves_destination_edits_and_deletions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("import.sqlite");
        let inputs = vec![imported(11, "First"), imported(22, "Second")];
        let receipt = {
            let metadata = Arc::new(MetadataStore::open(&path).unwrap());
            let accounts = AccountDirectory::new(metadata.clone()).unwrap();
            let before = accounts.snapshot().unwrap();
            metadata.migrate(&[MIGRATION, IMPORT_MIGRATION]).unwrap();
            let store = SavedSkinStore::new(metadata);
            let result = store
                .import_batch(SOURCE, FINGERPRINT, &inputs, &CancellationToken::new())
                .unwrap();
            assert!(!result.already_imported);
            assert_eq!(accounts.snapshot().unwrap(), before);
            for skin in &inputs {
                assert_eq!(
                    store.get(&skin.record.texture_key).unwrap().unwrap().record,
                    skin.record
                );
                assert_eq!(
                    store.read_png(&skin.record.texture_key).unwrap().unwrap(),
                    skin.png.as_ref()
                );
            }
            result.receipt
        };
        let metadata = Arc::new(MetadataStore::open(&path).unwrap());
        let store = SavedSkinStore::new(metadata.clone());
        assert_eq!(
            store.import_status(&receipt.skin_import_id).unwrap(),
            Some(receipt.clone())
        );
        store
            .update_metadata(
                &inputs[0].record.texture_key,
                Some("User edit".into()),
                None,
                None,
            )
            .unwrap();
        store
            .delete_unapplied(&inputs[1].record.texture_key)
            .unwrap();
        let before = store.list(None).unwrap();
        let before_revision = revision(&metadata);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let repeated = store
            .import_batch(SOURCE, FINGERPRINT, &inputs, &cancel)
            .unwrap();
        assert!(repeated.already_imported);
        assert_eq!(repeated.receipt, receipt);
        assert_eq!(store.list(None).unwrap(), before);
        assert_eq!(revision(&metadata), before_revision);
        assert!(
            store
                .read_png(&inputs[1].record.texture_key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn skin_import_initial_conflict_rolls_back_prior_insert_and_does_not_overwrite_metadata() {
        for field in [
            "name", "variant", "source", "cape", "created", "updated", "applied", "png",
        ] {
            let (metadata, store) = store();
            let bytes = png(11);
            let original = save(&store, &bytes, "Destination");
            store
                .mark_applied(&alice(), &original.texture_key, None)
                .unwrap();
            let mut record = original.clone();
            match field {
                "name" => record.name = "Source name".into(),
                "variant" => record.variant = SkinVariant::Slim,
                "source" => record.source = "minecraft_default_skin".into(),
                "cape" => record.cape_id = Some("source-cape".into()),
                "created" => record.created_at = "2024-01-01T00:00:00Z".into(),
                "updated" => record.updated_at = "2024-01-01T00:00:00Z".into(),
                "applied" => record.applied_at = Some("2024-01-01T00:00:00Z".into()),
                _ => {
                    let mut wrong = bytes.clone();
                    wrong[40] ^= 1;
                    metadata
                        .transaction(|tx| -> Result<_, StorageError> {
                            tx.execute(
                                "UPDATE saved_skins SET png=?1 WHERE texture_key=?2",
                                params![wrong, original.texture_key],
                            )?;
                            Ok(())
                        })
                        .unwrap();
                }
            }
            let before = store.list(Some(&alice())).unwrap();
            let before_revision = revision(&metadata);
            let inputs = [
                imported(22, "Must roll back"),
                prepare_import_record(record, bytes).unwrap(),
            ];
            assert_eq!(
                store.import_batch(SOURCE, FINGERPRINT, &inputs, &CancellationToken::new()),
                Err(SkinLibraryError::Conflict),
                "{field}"
            );
            assert_eq!(store.list(Some(&alice())).unwrap(), before, "{field}");
            assert_eq!(revision(&metadata), before_revision);
            assert!(
                store
                    .import_status(&skin_import_id(SOURCE, FINGERPRINT).unwrap())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn skin_import_reuses_only_exact_destination_and_keeps_live_account_markers() {
        let (metadata, store) = store();
        let bytes = png(11);
        let record = save(&store, &bytes, "Destination");
        store
            .mark_applied(&alice(), &record.texture_key, None)
            .unwrap();
        let inputs = [prepare_import_record(record, bytes).unwrap()];
        let before = store.list(Some(&alice())).unwrap();
        let before_revision = revision(&metadata);
        let completed = store
            .import_batch(SOURCE, FINGERPRINT, &inputs, &CancellationToken::new())
            .unwrap();
        assert!(!completed.already_imported);
        assert_eq!(store.list(Some(&alice())).unwrap(), before);
        assert_eq!(revision(&metadata), before_revision);
        assert_eq!(
            store.import_batch(SOURCE, SOURCE, &inputs, &CancellationToken::new()),
            Err(SkinLibraryError::Conflict)
        );
        let other_source = store
            .import_batch(FINGERPRINT, FINGERPRINT, &inputs, &CancellationToken::new())
            .unwrap();
        assert_ne!(
            other_source.receipt.skin_import_id,
            completed.receipt.skin_import_id
        );
        assert_eq!(revision(&metadata), before_revision);
    }

    #[test]
    fn skin_import_cancellation_before_and_during_transaction_leaves_no_partial_batch_or_receipt() {
        use rusqlite::hooks::Action;
        for cancellation_at in ["before", "saved_skins", "saved_skin_imports"] {
            let (metadata, store) = store();
            let cancel = CancellationToken::new();
            if cancellation_at == "before" {
                cancel.cancel();
            } else {
                let token = cancel.clone();
                metadata
                    .read(|connection| -> Result<_, StorageError> {
                        connection.update_hook(Some(
                            move |_: Action, _: &str, table: &str, _: i64| {
                                if table == cancellation_at {
                                    token.cancel();
                                }
                            },
                        ));
                        Ok(())
                    })
                    .unwrap();
            }
            let inputs = [imported(11, "First"), imported(22, "Second")];
            assert_eq!(
                store.import_batch(SOURCE, FINGERPRINT, &inputs, &cancel),
                Err(SkinLibraryError::Conflict)
            );
            assert!(cancel.is_cancelled());
            metadata
                .read(|connection| -> Result<_, StorageError> {
                    connection.update_hook(None::<fn(Action, &str, &str, i64)>);
                    Ok(())
                })
                .unwrap();
            assert!(store.list(None).unwrap().is_empty());
            assert_eq!(revision(&metadata), 0);
            assert!(
                store
                    .import_status(&skin_import_id(SOURCE, FINGERPRINT).unwrap())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn skin_import_failed_receipt_insert_rolls_back_every_png_and_revision() {
        let (metadata, store) = store();
        metadata.transaction(|tx| -> Result<_, StorageError> {
            tx.execute_batch("CREATE TRIGGER reject_skin_import BEFORE INSERT ON saved_skin_imports BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;")?;
            Ok(())
        }).unwrap();
        assert!(
            store
                .import_batch(
                    SOURCE,
                    FINGERPRINT,
                    &[imported(11, "First"), imported(22, "Second")],
                    &CancellationToken::new()
                )
                .is_err()
        );
        assert!(store.list(None).unwrap().is_empty());
        assert_eq!(revision(&metadata), 0);
        assert!(
            store
                .import_status(&skin_import_id(SOURCE, FINGERPRINT).unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn skin_import_empty_duplicate_and_receipt_validation_are_explicit() {
        let (metadata, store) = store();
        let input = imported(11, "First");
        assert_eq!(
            store.import_batch(
                SOURCE,
                FINGERPRINT,
                &[input.clone(), input.clone()],
                &CancellationToken::new()
            ),
            Err(SkinLibraryError::InvalidData)
        );
        assert_eq!(
            store.import_batch(
                SOURCE,
                FINGERPRINT,
                &vec![input; MAX_SAVED_SKINS + 1],
                &CancellationToken::new()
            ),
            Err(SkinLibraryError::Capacity)
        );
        let completed = store
            .import_batch(SOURCE, FINGERPRINT, &[], &CancellationToken::new())
            .unwrap();
        assert!(completed.receipt.texture_keys.is_empty());
        assert_eq!(revision(&metadata), 0);
        for keys in [
            format!("[\"{SOURCE}\",\"{SOURCE}\"]"),
            format!("[\"{FINGERPRINT}\",\"{SOURCE}\"]"),
            "[\"bad\"]".into(),
        ] {
            metadata
                .transaction(|tx| -> Result<_, StorageError> {
                    tx.execute("UPDATE saved_skin_imports SET texture_keys=?1", [keys])?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                store.import_status(&completed.receipt.skin_import_id),
                Err(SkinLibraryError::InvalidData)
            );
        }
    }

    fn prepare_scale_skin(index: u32) -> PreparedSkinRecord {
        let mut rgba = [0_u8, 20, 30, 255].repeat(64 * 64);
        // Mix 512 compact skins with 32,256 dense skins so both retained limits
        // are exercised without adding padding or changing encoded PNG bytes.
        if index % 64 != 0 {
            let mut random = (u64::from(index) + 1).wrapping_mul(0x9e3779b97f4a7c15);
            for bytes in rgba.chunks_exact_mut(8) {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                bytes.copy_from_slice(&random.to_le_bytes());
            }
        }
        // Base RGB survives normalization and makes every fixture distinct.
        rgba[..3].copy_from_slice(&index.to_le_bytes()[..3]);
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }
        let normalized = crate::media::normalize_skin_png(&bytes).unwrap();
        prepare_import_record(
            SavedSkinRecord {
                texture_key: texture_key(&normalized.png_bytes),
                name: format!("Scale skin {index}"),
                variant: normalized.variant_suggestion,
                source: "local_file".into(),
                cape_id: None,
                created_at: "2024-01-01T00:00:00Z".into(),
                updated_at: "2024-01-02T00:00:00Z".into(),
                applied_at: None,
                byte_size: normalized.png_bytes.len(),
            },
            normalized.png_bytes,
        )
        .unwrap()
    }

    #[test]
    fn skin_import_scale_fixture_fits_both_limits() {
        let mut keys = BTreeSet::new();
        let mut dense_min = usize::MAX;
        let mut dense_max = 0;
        let mut compact_min = usize::MAX;
        let mut compact_max = 0;
        for index in (0..128).chain(MAX_SAVED_SKINS as u32 - 128..MAX_SAVED_SKINS as u32) {
            let skin = prepare_scale_skin(index);
            assert!(keys.insert(skin.record.texture_key));
            let size = skin.png.len();
            if index % 64 == 0 {
                compact_min = compact_min.min(size);
                compact_max = compact_max.max(size);
                assert!(size <= 1_024);
            } else {
                dense_min = dense_min.min(size);
                dense_max = dense_max.max(size);
                assert!(size <= 16_516);
            }
        }
        let compact_count = MAX_SAVED_SKINS / 64;
        let dense_count = MAX_SAVED_SKINS - compact_count;
        let projected_min = dense_count * dense_min + compact_count * compact_min;
        let projected_max = dense_count * dense_max + compact_count * compact_max;
        eprintln!(
            "skin import scale fixture: samples={} dense_min={dense_min} dense_max={dense_max} compact_min={compact_min} compact_max={compact_max} projected_min={projected_min} projected_max={projected_max}",
            keys.len()
        );
        assert!(projected_min >= 384 * 1024 * 1024);
        assert!(projected_max as u64 <= MAX_TOTAL_PNG_BYTES);
    }

    #[test]
    #[ignore = "opt-in maximum-count import with hundreds of MiB of in-memory PNGs"]
    fn skin_import_maximum_scale_under_default_budget() {
        let started = std::time::Instant::now();
        let inputs: Vec<_> = (0..MAX_SAVED_SKINS as u32)
            .map(prepare_scale_skin)
            .collect();
        let total_bytes: u64 = inputs.iter().map(|skin| skin.png.len() as u64).sum();
        let minimum_bytes = inputs.iter().map(|skin| skin.png.len()).min().unwrap();
        let maximum_bytes = inputs.iter().map(|skin| skin.png.len()).max().unwrap();
        eprintln!(
            "skin import scale preparation: count={} bytes={total_bytes} png_min={minimum_bytes} png_max={maximum_bytes} elapsed_ms={}",
            inputs.len(),
            started.elapsed().as_millis()
        );
        assert!(total_bytes >= 384 * 1024 * 1024);
        assert!(total_bytes <= MAX_TOTAL_PNG_BYTES);

        for count in [2_048, 8_192, MAX_SAVED_SKINS] {
            let batch = &inputs[..count];
            let bytes: u64 = batch.iter().map(|skin| skin.png.len() as u64).sum();
            let (metadata, store) = store();
            let cancel = CancellationToken::new();
            // Includes receipt preparation; the unchanged transaction budget is
            // enforced by MetadataStore::in_memory(), independently of this timer.
            let started = std::time::Instant::now();
            let result = store.import_batch(SOURCE, FINGERPRINT, batch, &cancel);
            eprintln!(
                "skin import scale: count={count} bytes={bytes} import_elapsed_ms={} outcome={:?}",
                started.elapsed().as_millis(),
                result.as_ref().map(|commit| commit.already_imported)
            );
            let commit = result.expect("bounded skin import must fit the default metadata budget");
            assert!(!commit.already_imported);
            let expected_keys: BTreeSet<_> = batch
                .iter()
                .map(|skin| skin.record.texture_key.clone())
                .collect();
            assert_eq!(expected_keys.len(), count);
            assert_eq!(
                commit.receipt.texture_keys,
                expected_keys.into_iter().collect::<Vec<_>>()
            );
            let stored: (usize, u64) = metadata
                .read(|connection| -> Result<_, StorageError> {
                    Ok(connection.query_row(
                        "SELECT COUNT(*),COALESCE(SUM(byte_size),0) FROM saved_skins",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?)
                })
                .unwrap();
            assert_eq!(stored, (count, bytes));
            assert_eq!(revision(&metadata), count as i64);
            assert_eq!(
                store.import_status(&commit.receipt.skin_import_id).unwrap(),
                Some(commit.receipt.clone())
            );
            for index in [0, count / 2, count - 1] {
                let skin = &batch[index];
                assert_eq!(
                    store.get(&skin.record.texture_key).unwrap().unwrap().record,
                    skin.record
                );
                assert_eq!(
                    store.read_png(&skin.record.texture_key).unwrap().unwrap(),
                    skin.png.as_ref()
                );
            }
            let started = std::time::Instant::now();
            let replay = store
                .import_batch(SOURCE, FINGERPRINT, batch, &cancel)
                .unwrap();
            eprintln!(
                "skin import scale replay: count={count} elapsed_ms={}",
                started.elapsed().as_millis()
            );
            assert!(replay.already_imported);
            assert_eq!(replay.receipt, commit.receipt);
            assert_eq!(revision(&metadata), count as i64);
        }
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
        {
            let metadata = Arc::new(MetadataStore::open(&path).unwrap());
            accounts(metadata.clone());
            metadata.migrate(&[MIGRATION]).unwrap();
            let store = SavedSkinStore::new(metadata);
            key = save(&store, &bytes, "A retained skin").texture_key;
            save(&store, &bytes, "Renamed without a duplicate");
            store.mark_applied(&alice(), &key, None).unwrap();
        }
        let metadata = Arc::new(MetadataStore::open(&path).unwrap());
        metadata.migrate(&[MIGRATION]).unwrap();
        let reopened = SavedSkinStore::new(metadata);
        let records = reopened.list(Some(&alice())).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "Renamed without a duplicate");
        assert!(records[0].applied_at.is_some());
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
        store
            .update_metadata(&first.texture_key, Some("Edited".into()), None, None)
            .unwrap();
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

    #[test]
    fn imported_applied_time_is_preserved_without_inventing_active_account_ownership() {
        let (_, store) = store();
        let active = save(&store, &png(11), "Actually applied");
        store
            .mark_applied(&bob(), &active.texture_key, None)
            .unwrap();
        let bytes = png(22);
        let mut historical = active.clone();
        historical.texture_key = texture_key(&bytes);
        historical.name = "Imported history".into();
        historical.byte_size = bytes.len();
        historical.applied_at = Some(historical.created_at.clone());
        let imported = prepare_import_record(historical.clone(), bytes).unwrap();
        assert!(
            !store
                .import_batch(SOURCE, FINGERPRINT, &[imported], &CancellationToken::new())
                .unwrap()
                .already_imported
        );
        assert_eq!(
            store
                .get(&historical.texture_key)
                .unwrap()
                .unwrap()
                .record
                .applied_at,
            historical.applied_at
        );
        assert!(
            store
                .list(Some(&alice()))
                .unwrap()
                .iter()
                .all(|record| record.applied_at.is_none())
        );
        assert!(matches!(
            store.delete_unapplied(&historical.texture_key).unwrap(),
            SavedSkinDeleteResult::Deleted(_)
        ));
        assert!(matches!(
            store.delete_unapplied(&active.texture_key).unwrap(),
            SavedSkinDeleteResult::Applied
        ));
    }
}
