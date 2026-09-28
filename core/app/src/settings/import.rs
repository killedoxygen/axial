use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    ConfigPatch, ConfigView, MAX_DOCUMENT_BYTES, SettingsCommit, SettingsError, SettingsStore,
    flags, required_document, write_updated_document,
};
use crate::storage::rusqlite;

/// Validated preview only. The import owner must separately admit library data,
/// convert account selection and provide an atomic destination commit receipt.
#[derive(Clone, Debug)]
pub struct PreparedSettingsImport {
    pub config: ConfigView,
    pub feature_overrides: BTreeMap<String, bool>,
    pub excluded_library_metadata: bool,
}

impl SettingsStore {
    /// The import owner writes accounts and its completed receipt here. Returning
    /// false preserves later destination edits without testing the old revision.
    /// Verify the import receipt after the final settings write, before commit.
    pub(crate) fn commit_prepared_import<T>(
        &self,
        prepared: &PreparedSettingsImport,
        expected_revision: u64,
        import_metadata: impl FnOnce(
            &rusqlite::Transaction<'_>,
            &mut ConfigView,
        ) -> Result<(bool, T), SettingsError>,
        verify_import: impl FnOnce(&rusqlite::Transaction<'_>, &T) -> Result<(), SettingsError>,
    ) -> Result<(SettingsCommit, bool, T), SettingsError> {
        let changes = self
            .changes
            .lock()
            .map_err(|_| SettingsError::Unavailable)?;
        let (document, imported, receipt) = self.metadata.transaction(|transaction| {
            let mut document = required_document(transaction)?;
            let mut config = prepared.config.clone();
            let (imported, receipt) = import_metadata(transaction, &mut config)?;
            if imported {
                if document.config.revision != expected_revision {
                    return Err(SettingsError::Conflict);
                }
                document.config = config;
                document.feature_overrides = prepared.feature_overrides.clone();
                if document.config.telemetry_enabled && self.telemetry_identity_enabled {
                    document
                        .telemetry_install_id
                        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
                } else if !document.config.telemetry_enabled {
                    document.telemetry_install_id = None;
                }
                write_updated_document(transaction, &mut document, expected_revision)?;
            }
            verify_import(transaction, &receipt)?;
            Ok::<_, SettingsError>((document, imported, receipt))
        })?;
        if imported {
            changes.send_replace(document.config.clone());
        }
        Ok((
            SettingsCommit {
                config: document.config,
                telemetry_identity: document.telemetry_install_id,
            },
            imported,
            receipt,
        ))
    }
}

pub fn prepare_legacy_import(value: &Value) -> Result<PreparedSettingsImport, SettingsError> {
    let invalid =
        || SettingsError::Validation("Legacy settings are invalid or contain unsupported fields.");
    if serde_json::to_vec(value).map_err(|_| invalid())?.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid());
    }
    let mut fields = value.as_object().cloned().ok_or_else(invalid)?;
    // These were required even before optional preferences were introduced.
    if ["username", "max_memory_mb", "min_memory_mb"]
        .iter()
        .any(|key| !fields.contains_key(*key))
    {
        return Err(invalid());
    }
    for field in [
        "guardian_mode",
        "guardian_idle_integrity_enabled",
        "telemetry_install_id",
    ] {
        fields.remove(field);
    }
    let mut excluded_library_metadata = false;
    for field in ["library_dir", "library_mode"] {
        if let Some(value) = fields.remove(field) {
            let value = value.as_str().ok_or_else(invalid)?;
            excluded_library_metadata |= !value.is_empty();
        }
    }
    let feature_overrides: BTreeMap<String, bool> = fields
        .remove("feature_overrides")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| invalid())?
        .unwrap_or_default();
    if feature_overrides.keys().any(|key| !flags::known_flag(key)) {
        return Err(invalid());
    }
    if [
        "expected_revision",
        "revision",
        "expected_account_selection_revision",
        "account_selection_revision",
    ]
    .iter()
    .any(|key| fields.contains_key(*key))
    {
        return Err(invalid());
    }
    fields.insert("expected_revision".into(), Value::from(0));
    let mut patch: ConfigPatch =
        serde_json::from_value(Value::Object(fields)).map_err(|_| invalid())?;
    // Import restores a provider-owned online name, not an offline rename
    // command. Keep ConfigPatch's stricter offline edit validation unchanged.
    let imported_name = if patch.launch_auth_mode == Some(super::ConfigLaunchAuthMode::Online) {
        patch.username.take()
    } else {
        None
    };
    let mut config = ConfigView::default();
    patch.apply(&mut config)?;
    if let Some(name) = imported_name {
        config.username = name;
    }
    config.validate()?;
    Ok(PreparedSettingsImport {
        config,
        feature_overrides,
        excluded_library_metadata,
    })
}
