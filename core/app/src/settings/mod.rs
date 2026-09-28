//! Revisioned preferences and feature flags. Instance records own their overrides;
//! library lifecycle owns every root selection and its filesystem authority.

mod flags;
mod import;
mod model;
mod preferences;

pub use flags::{
    FlagOverridePatch, FlagSource, FlagStage, FlagViewModel, FlagsResponse, STATE_INSPECTOR_FLAG,
};
pub use import::{PreparedSettingsImport, prepare_legacy_import};
pub use model::{
    ConfigJvmPreset, ConfigLaunchAuthMode, ConfigPatch, ConfigPerformanceMode, ConfigTheme,
    ConfigView, EffectiveLaunchSettings, InstanceSettings, NullablePatch, validate_username,
};
pub use preferences::{
    InterfacePreferences, InterfacePreferencesChange, InterfacePreferencesReceipt,
    InterfacePreferencesSnapshot, InterfacePreferencesUpdate, InterfaceRoute, LocalPreferences,
    MAX_INTERFACE_PREFERENCES_BYTES, OverlayPosition, ShortcutBinding,
};

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::storage::{
    MetadataStore, Migration, StorageError,
    rusqlite::{self, OptionalExtension, params},
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// The composition may register this migration before constructing settings.
pub const SETTINGS_MIGRATION: Migration = Migration {
    id: "settings_v1",
    sql: "CREATE TABLE settings_config (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        revision INTEGER NOT NULL CHECK(revision >= 0),
        document TEXT NOT NULL
    );",
};

const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_STORED_DOCUMENT_BYTES: usize = MAX_DOCUMENT_BYTES + MAX_INTERFACE_PREFERENCES_BYTES + 256;
const MAX_REVISION: u64 = (1_u64 << 53) - 1;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Validation(&'static str),
    #[error("Settings changed. Reload and try again.")]
    Conflict,
    #[error("Unknown feature flag.")]
    UnknownFlag,
    #[error("Saved settings are unreadable. Existing data has been preserved.")]
    Corrupt,
    #[error("Could not save settings. Check app data permissions and try again.")]
    Storage(#[from] StorageError),
    #[error("Settings are temporarily unavailable.")]
    Unavailable,
}

impl From<rusqlite::Error> for SettingsError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    config: ConfigView,
    feature_overrides: BTreeMap<String, bool>,
    telemetry_install_id: Option<String>,
    #[serde(default)]
    interface_preferences_revision: u64,
    #[serde(default)]
    interface_preferences: Option<InterfacePreferences>,
}

impl Document {
    fn validate(&self) -> Result<(), SettingsError> {
        self.config.validate().map_err(|_| SettingsError::Corrupt)?;
        if self.config.revision > MAX_REVISION
            || self.interface_preferences_revision > MAX_REVISION
            || self
                .feature_overrides
                .keys()
                .any(|key| !flags::known_flag(key))
        {
            return Err(SettingsError::Corrupt);
        }
        if let Some(preferences) = &self.interface_preferences {
            preferences.validate().map_err(|_| SettingsError::Corrupt)?;
        }
        match (self.config.telemetry_enabled, &self.telemetry_install_id) {
            (true, Some(id))
                if uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == *id) =>
            {
                Ok(())
            }
            (true, None) => Ok(()),
            (false, None) => Ok(()),
            _ => Err(SettingsError::Corrupt),
        }
    }
}

pub struct SettingsStore {
    metadata: Arc<MetadataStore>,
    telemetry_identity_enabled: bool,
    // Couples a committed local mutation and its public notification; no mutex
    // or metadata transaction is held across network/process/filesystem I/O.
    changes: Mutex<watch::Sender<ConfigView>>,
}

/// Private commit result for the workflow that owns telemetry consent admission.
/// This identity is never serialized into the public settings response.
pub struct SettingsCommit {
    pub config: ConfigView,
    pub telemetry_identity: Option<String>,
}

impl SettingsStore {
    pub fn new(metadata: Arc<MetadataStore>) -> Result<Self, SettingsError> {
        Self::new_with_telemetry_identity(metadata, false)
    }

    /// Composition supplies exporter availability, without coupling persistence
    /// to the telemetry runtime. Keyless profiles retain their consent preference.
    pub fn new_with_telemetry_identity(
        metadata: Arc<MetadataStore>,
        exporter_configured: bool,
    ) -> Result<Self, SettingsError> {
        metadata.migrate(&[SETTINGS_MIGRATION])?;
        let config = metadata.transaction(|tx| -> Result<_, SettingsError> {
            match read_document(tx)? {
                Some(mut document) => {
                    if exporter_configured && document.config.telemetry_enabled && document.telemetry_install_id.is_none() {
                        document.telemetry_install_id = Some(uuid::Uuid::new_v4().to_string());
                        document.config.revision = next_revision(document.config.revision)?;
                        tx.execute("UPDATE settings_config SET revision=?1, document=?2 WHERE singleton=1",
                            params![document.config.revision, encode(&document)?])?;
                    }
                    Ok(document.config)
                }
                None => {
                    let document = Document::default();
                    tx.execute("INSERT INTO settings_config(singleton, revision, document) VALUES(1, 0, ?1)",
                        [encode(&document)?])?;
                    Ok(document.config)
                }
            }
        })?;
        let (changes, _) = watch::channel(config);
        Ok(Self {
            metadata,
            telemetry_identity_enabled: exporter_configured,
            changes: Mutex::new(changes),
        })
    }

    pub fn metadata(&self) -> &Arc<MetadataStore> {
        &self.metadata
    }

    pub fn telemetry_identity_enabled(&self) -> bool {
        self.telemetry_identity_enabled
    }

    pub fn current(&self) -> Result<ConfigView, SettingsError> {
        self.metadata
            .read(|connection| Ok(required_document(connection)?.config))
    }

    /// Read launch preferences inside an owning feature's metadata transaction.
    pub(crate) fn current_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<ConfigView, SettingsError> {
        Ok(required_document(transaction)?.config)
    }

    /// A cooperating owner may project its authoritative fields under the same
    /// metadata read lock without a nested store access.
    pub fn current_with_projection(
        &self,
        project: impl FnOnce(&rusqlite::Connection, &mut ConfigView) -> Result<(), SettingsError>,
    ) -> Result<ConfigView, SettingsError> {
        self.metadata.read(|connection| {
            let transaction = connection.unchecked_transaction()?;
            let mut config = required_document(&transaction)?.config;
            project(&transaction, &mut config)?;
            config.validate()?;
            transaction.commit()?;
            Ok(config)
        })
    }

    pub fn validate_revision(&self, revision: u64) -> Result<(), SettingsError> {
        if self.current()?.revision == revision {
            Ok(())
        } else {
            Err(SettingsError::Conflict)
        }
    }

    /// Read and subscribe under the same mutation gate. The receiver starts with
    /// the latest committed revision and coalesces later snapshots without gaps.
    pub fn subscribe(&self) -> Result<watch::Receiver<ConfigView>, SettingsError> {
        let changes = self
            .changes
            .lock()
            .map_err(|_| SettingsError::Unavailable)?;
        changes.send_replace(self.current()?);
        Ok(changes.subscribe())
    }

    pub fn update(&self, patch: ConfigPatch) -> Result<ConfigView, SettingsError> {
        self.update_with_transaction(patch, |_, _| Ok(()))
    }

    /// The account owner uses this transaction seam to commit selected offline
    /// username changes together with the settings revision. Owner errors roll
    /// back both records; callers must not perform nested storage calls here.
    pub fn update_with_transaction(
        &self,
        patch: ConfigPatch,
        owner_update: impl FnOnce(
            &rusqlite::Transaction<'_>,
            &mut ConfigView,
        ) -> Result<(), SettingsError>,
    ) -> Result<ConfigView, SettingsError> {
        self.commit_with_transaction(patch, owner_update)
            .map(|commit| commit.config)
    }

    pub fn commit_with_transaction(
        &self,
        patch: ConfigPatch,
        owner_update: impl FnOnce(
            &rusqlite::Transaction<'_>,
            &mut ConfigView,
        ) -> Result<(), SettingsError>,
    ) -> Result<SettingsCommit, SettingsError> {
        self.mutate(patch.expected_revision, |tx, document| {
            patch.apply(&mut document.config)?;
            owner_update(tx, &mut document.config)?;
            document.config.validate()?;
            if document.config.telemetry_enabled && self.telemetry_identity_enabled {
                document
                    .telemetry_install_id
                    .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
            } else if !document.config.telemetry_enabled {
                document.telemetry_install_id = None;
            }
            Ok(())
        })
        .map(|document| SettingsCommit {
            config: document.config,
            telemetry_identity: document.telemetry_install_id,
        })
    }

    /// Only the telemetry owner needs this private identity. Disabled consent
    /// clears it durably; turning consent back on creates a fresh identity.
    pub fn telemetry_identity(&self) -> Result<Option<String>, SettingsError> {
        self.metadata
            .read(|connection| Ok(required_document(connection)?.telemetry_install_id))
    }

    pub fn list_flags(&self) -> Result<FlagsResponse, SettingsError> {
        self.list_flags_for_build(cfg!(debug_assertions))
    }

    fn list_flags_for_build(
        &self,
        development_build: bool,
    ) -> Result<FlagsResponse, SettingsError> {
        self.metadata.read(|connection| {
            let document = required_document(connection)?;
            Ok(flags::project(
                document.config.revision,
                &document.feature_overrides,
                development_build,
            ))
        })
    }

    pub fn update_flag(
        &self,
        key: &str,
        patch: FlagOverridePatch,
    ) -> Result<FlagsResponse, SettingsError> {
        self.update_flag_for_build(key, patch, cfg!(debug_assertions))
    }

    fn update_flag_for_build(
        &self,
        key: &str,
        patch: FlagOverridePatch,
        development_build: bool,
    ) -> Result<FlagsResponse, SettingsError> {
        if !flags::flag_visible(key, development_build) {
            return Err(SettingsError::UnknownFlag);
        }
        let document = self.mutate(patch.expected_revision, |_, document| {
            if let Some(enabled) = patch.enabled {
                document.feature_overrides.insert(key.into(), enabled);
            } else {
                document.feature_overrides.remove(key);
            }
            Ok(())
        })?;
        Ok(flags::project(
            document.config.revision,
            &document.feature_overrides,
            development_build,
        ))
    }

    fn mutate(
        &self,
        expected_revision: u64,
        apply: impl FnOnce(&rusqlite::Transaction<'_>, &mut Document) -> Result<(), SettingsError>,
    ) -> Result<Document, SettingsError> {
        let changes = self
            .changes
            .lock()
            .map_err(|_| SettingsError::Unavailable)?;
        let document = self
            .metadata
            .transaction(|tx| -> Result<_, SettingsError> {
                let mut document = required_document(tx)?;
                if document.config.revision != expected_revision {
                    return Err(SettingsError::Conflict);
                }
                apply(tx, &mut document)?;
                write_updated_document(tx, &mut document, expected_revision)?;
                Ok(document)
            })?;
        changes.send_replace(document.config.clone());
        Ok(document)
    }
}

fn write_updated_document(
    transaction: &rusqlite::Transaction<'_>,
    document: &mut Document,
    expected_revision: u64,
) -> Result<(), SettingsError> {
    document.config.revision = next_revision(expected_revision)?;
    document.validate()?;
    let changed = transaction.execute(
        "UPDATE settings_config SET revision = ?1, document = ?2 WHERE singleton = 1 AND revision = ?3",
        params![document.config.revision, encode(document)?, expected_revision],
    )?;
    if changed != 1 {
        return Err(SettingsError::Conflict);
    }
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, SettingsError> {
    revision
        .checked_add(1)
        .filter(|next| *next <= MAX_REVISION)
        .ok_or(SettingsError::Unavailable)
}

fn read_document(connection: &rusqlite::Connection) -> Result<Option<Document>, SettingsError> {
    // Reject oversized/corrupt data before allocating its contents. SQLite's
    // length(blob) counts bytes, including embedded NULs.
    let row = connection.query_row(
        "SELECT revision, length(CAST(document AS BLOB)), CASE WHEN length(CAST(document AS BLOB)) <= ?1 THEN document END FROM settings_config WHERE singleton = 1",
        [MAX_STORED_DOCUMENT_BYTES], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, Option<String>>(2)?))
    ).optional()?;
    let Some((revision, length, encoded)) = row else {
        return Ok(None);
    };
    if revision < 0 || length < 0 || length as usize > MAX_STORED_DOCUMENT_BYTES {
        return Err(SettingsError::Corrupt);
    }
    let document: Document = serde_json::from_str(&encoded.ok_or(SettingsError::Corrupt)?)
        .map_err(|_| SettingsError::Corrupt)?;
    if document.config.revision != revision as u64 {
        return Err(SettingsError::Corrupt);
    }
    document.validate()?;
    Ok(Some(document))
}

fn required_document(connection: &rusqlite::Connection) -> Result<Document, SettingsError> {
    read_document(connection)?.ok_or(SettingsError::Corrupt)
}

fn encode(document: &Document) -> Result<String, SettingsError> {
    let value = serde_json::to_string(document).map_err(|_| SettingsError::Unavailable)?;
    if value.len() > MAX_STORED_DOCUMENT_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
