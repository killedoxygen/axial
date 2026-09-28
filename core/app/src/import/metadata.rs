//! One atomic account/settings conversion. The completed receipt is an
//! idempotency fact, not a journal or an attestation of full profile cutover.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    ImportError, ImportResult, Inventory,
    model::{
        MetadataImportReceipt, MetadataImportRequest, MetadataImportResponse, MetadataImportStatus,
    },
};
use crate::{
    accounts::{
        directory::AccountDirectory,
        model::{AccountError, AccountId, MicrosoftIdentityImport, OfflineIdentityImport},
    },
    library::ApplicationRootPin,
    settings::{
        ConfigLaunchAuthMode, PreparedSettingsImport, SettingsCommit, SettingsError, SettingsStore,
        prepare_legacy_import,
    },
    storage::{
        Migration,
        rusqlite::{Connection, OptionalExtension, params},
    },
    tasks::CancellationToken,
};

pub const METADATA_IMPORT_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v1",
    sql: "CREATE TABLE profile_metadata_imports (
        source_id TEXT PRIMARY KEY NOT NULL,
        fingerprint TEXT NOT NULL CHECK(length(fingerprint) = 64),
        import_id TEXT NOT NULL UNIQUE CHECK(length(import_id) = 64),
        account_count INTEGER NOT NULL CHECK(account_count BETWEEN 1 AND 256),
        settings_revision INTEGER NOT NULL CHECK(settings_revision > 0),
        selection_revision INTEGER NOT NULL CHECK(selection_revision >= 0)
    );",
};

pub const METADATA_IMPORT_IDENTITIES_MIGRATION: Migration = Migration {
    id: "profile_metadata_imports.v2",
    sql: "ALTER TABLE profile_metadata_imports ADD COLUMN microsoft_account_count INTEGER NOT NULL DEFAULT 0
        CHECK(microsoft_account_count BETWEEN 0 AND account_count);
    ALTER TABLE profile_metadata_imports ADD COLUMN account_id_mapping TEXT
        CHECK(account_id_mapping IS NULL OR length(CAST(account_id_mapping AS BLOB)) <= 131072);",
};

const MAX_MAPPING_BYTES: usize = 131072;

#[derive(Debug, thiserror::Error)]
pub enum MetadataImportError {
    #[error(transparent)]
    Source(#[from] ImportError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

/// Only a source inventory can construct this value. Caller-authored records or
/// paths never authorize a metadata import, and forgetting a preview does not
/// discard the retained source authority of already accepted work.
#[derive(Clone)]
pub struct PreparedMetadataImport {
    inventory: Arc<Inventory>,
    source_id: String,
    import_id: String,
    accounts: PreparedAccounts,
    settings: PreparedSettingsImport,
}

/// Telemetry consent is published by the accepted command's owned consent
/// guard. On receipt replay, settings contains CURRENT persisted preferences
/// and replacement identity, never the original import's stale consent.
pub struct MetadataImportCommit {
    pub response: MetadataImportResponse,
    pub settings: SettingsCommit,
}

impl Inventory {
    pub(super) fn metadata_import_id(&self) -> ImportResult<String> {
        Ok(import_id(&self.source_identity()?, self.fingerprint()))
    }

    pub(super) fn metadata_import_available(&self) -> bool {
        self.metadata_inputs().is_ok()
    }

    pub(super) fn prepare_metadata(
        self: &Arc<Self>,
        fingerprint: &str,
    ) -> ImportResult<PreparedMetadataImport> {
        self.revalidate()?;
        if self.fingerprint() != fingerprint {
            return Err(ImportError::SourceChanged);
        }
        let (settings, accounts) = self.metadata_inputs()?;
        let source_id = self.source_identity()?;
        self.revalidate()?;
        Ok(PreparedMetadataImport {
            inventory: Arc::clone(self),
            import_id: import_id(&source_id, fingerprint),
            source_id,
            accounts,
            settings,
        })
    }

    fn metadata_inputs(&self) -> ImportResult<(PreparedSettingsImport, PreparedAccounts)> {
        let settings = prepare_legacy_import(
            &serde_json::from_slice(&self.record_bytes("profile/config.json")?)
                .map_err(|_| ImportError::InvalidData)?,
        )
        .map_err(|_| ImportError::InvalidData)?;
        let accounts = prepare_accounts(self, &settings)?;
        Ok((settings, accounts))
    }
}

impl PreparedMetadataImport {
    /// Run in blocking accepted work while holding telemetry's consent guard.
    /// All filesystem validation precedes the short metadata-only transaction.
    pub fn commit(
        &self,
        settings: &SettingsStore,
        accounts: &AccountDirectory,
        destination: &ApplicationRootPin,
        request: &MetadataImportRequest,
        cancel: &CancellationToken,
    ) -> Result<MetadataImportCommit, MetadataImportError> {
        if !accounts.uses_metadata(settings.metadata()) {
            return Err(SettingsError::Unavailable.into());
        }
        if request.fingerprint != self.inventory.fingerprint()
            || request.metadata_import_id != self.import_id
        {
            return Err(ImportError::SourceChanged.into());
        }
        check_cancel(cancel)?;
        let root = destination.directory().map_err(ImportError::Io)?;
        self.inventory.validate_destination_root(&root)?;
        check_cancel(cancel)?;
        let mut receipt = None;
        let result = settings.commit_prepared_import(
            &self.settings,
            request.expected_settings_revision,
            |transaction, config| {
                let existing = transaction.query_row(
                    "SELECT fingerprint, import_id FROM profile_metadata_imports WHERE source_id = ?1",
                    [&self.source_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                ).optional()?;
                if let Some((fingerprint, id)) = existing {
                    if fingerprint != self.inventory.fingerprint() || id != self.import_id {
                        return Err(SettingsError::Conflict);
                    }
                    receipt = Some(read_receipt(transaction, &id)?.ok_or(SettingsError::Corrupt)?);
                    return Ok(false);
                }
                if cancel.is_cancelled() {
                    return Err(SettingsError::Unavailable);
                }
                let snapshot = AccountDirectory::import_identities_in_transaction(
                    transaction,
                    &self.accounts.offline,
                    &self.accounts.microsoft,
                    &self.accounts.active_id,
                    request.expected_account_selection_revision,
                ).map_err(account_error)?;
                config.account_selection_revision = snapshot.selection_revision;
                let completed = MetadataImportReceipt {
                    metadata_import_id: self.import_id.clone(),
                    imported_offline_account_count: self.accounts.offline.len(),
                    imported_microsoft_account_count: self.accounts.microsoft.len(),
                    account_id_mapping: Some(self.accounts.mapping.clone()),
                    settings_revision: request.expected_settings_revision.checked_add(1)
                        .ok_or(SettingsError::Unavailable)?,
                    account_selection_revision: snapshot.selection_revision,
                };
                transaction.execute(
                    "INSERT INTO profile_metadata_imports(source_id, fingerprint, import_id, account_count, settings_revision, selection_revision, microsoft_account_count, account_id_mapping) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![self.source_id, self.inventory.fingerprint(), self.import_id,
                        self.accounts.mapping.len(), completed.settings_revision,
                        completed.account_selection_revision, completed.imported_microsoft_account_count,
                        encode_mapping(&self.accounts.mapping)?],
                )?;
                if cancel.is_cancelled() {
                    return Err(SettingsError::Unavailable);
                }
                receipt = Some(completed);
                Ok(true)
            },
        );
        // Cancellation may win before persistence, never after a successful
        // commit. Returning cancellation after commit would hide its outcome.
        let (settings, imported) = match result {
            Err(SettingsError::Unavailable) if cancel.is_cancelled() => {
                return Err(ImportError::Cancelled.into());
            }
            result => result?,
        };
        Ok(MetadataImportCommit {
            response: MetadataImportResponse {
                receipt: receipt.ok_or(SettingsError::Corrupt)?,
                already_imported: !imported,
                cutover_available: false,
            },
            settings,
        })
    }
}

/// A source-independent read reconciles an unknown HTTP outcome, even if the
/// preview was forgotten or the old volume is no longer connected.
pub fn metadata_status(
    settings: &SettingsStore,
    import_id: &str,
) -> Result<MetadataImportStatus, MetadataImportError> {
    if import_id.len() != 64
        || !import_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ImportError::InvalidData.into());
    }
    let receipt = settings
        .metadata()
        .read(|connection| read_receipt(connection, import_id))?;
    Ok(MetadataImportStatus {
        receipt,
        cutover_available: false,
    })
}

fn read_receipt(
    connection: &Connection,
    import_id: &str,
) -> Result<Option<MetadataImportReceipt>, SettingsError> {
    let row = connection
        .query_row(
            "SELECT account_count, settings_revision, selection_revision, microsoft_account_count,
            length(CAST(account_id_mapping AS BLOB)),
            CASE WHEN length(CAST(account_id_mapping AS BLOB)) <= ?2 THEN account_id_mapping END
         FROM profile_metadata_imports WHERE import_id = ?1",
            params![import_id, MAX_MAPPING_BYTES],
            |row| {
                Ok((
                    row.get::<_, usize>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, usize>(3)?,
                    row.get::<_, Option<usize>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        count,
        settings_revision,
        account_selection_revision,
        microsoft_count,
        length,
        encoded,
    )) = row
    else {
        return Ok(None);
    };
    if !(1..=256).contains(&count)
        || microsoft_count > count
        || settings_revision == 0
        || settings_revision > 9_007_199_254_740_991
        || account_selection_revision > 9_007_199_254_740_991
        || length.is_some_and(|length| length > MAX_MAPPING_BYTES)
    {
        return Err(SettingsError::Corrupt);
    }
    let mapping = match (length, encoded) {
        (None, None) if microsoft_count == 0 => None,
        (Some(_), Some(encoded)) => Some(decode_mapping(&encoded, count, microsoft_count)?),
        _ => return Err(SettingsError::Corrupt),
    };
    Ok(Some(MetadataImportReceipt {
        metadata_import_id: import_id.to_owned(),
        imported_offline_account_count: count - microsoft_count,
        imported_microsoft_account_count: microsoft_count,
        account_id_mapping: mapping,
        settings_revision,
        account_selection_revision,
    }))
}

fn encode_mapping(mapping: &BTreeMap<String, String>) -> Result<String, SettingsError> {
    // Persist pairs so decoding rejects duplicate keys rather than silently
    // accepting JSON object key replacement. Public projection remains a map.
    let encoded = serde_json::to_string(&mapping.iter().collect::<Vec<_>>())
        .map_err(|_| SettingsError::Unavailable)?;
    if encoded.len() > MAX_MAPPING_BYTES {
        return Err(SettingsError::Unavailable);
    }
    Ok(encoded)
}

fn decode_mapping(
    encoded: &str,
    count: usize,
    microsoft_count: usize,
) -> Result<BTreeMap<String, String>, SettingsError> {
    let pairs: Vec<(String, String)> =
        serde_json::from_str(encoded).map_err(|_| SettingsError::Corrupt)?;
    if pairs.len() != count {
        return Err(SettingsError::Corrupt);
    }
    let mut mapping = BTreeMap::new();
    let mut destinations = HashSet::new();
    let mut microsoft = 0;
    for (source, destination) in pairs {
        if source.starts_with("offline-") {
            if source != destination || AccountId::parse(&source).is_err() {
                return Err(SettingsError::Corrupt);
            }
        } else {
            let parsed = uuid::Uuid::parse_str(&destination).map_err(|_| SettingsError::Corrupt)?;
            if !legacy_microsoft_id(&source)
                || parsed.is_nil()
                || parsed.hyphenated().to_string() != destination
            {
                return Err(SettingsError::Corrupt);
            }
            microsoft += 1;
        }
        if !destinations.insert(destination.clone())
            || mapping.insert(source, destination).is_some()
        {
            return Err(SettingsError::Corrupt);
        }
    }
    if microsoft != microsoft_count {
        return Err(SettingsError::Corrupt);
    }
    Ok(mapping)
}

fn import_id(source_id: &str, fingerprint: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"axial-profile-metadata-import-v1\0");
    hash.update(source_id.as_bytes());
    hash.update([0]);
    hash.update(fingerprint.as_bytes());
    hex::encode(hash.finalize())
}

fn check_cancel(cancel: &CancellationToken) -> ImportResult<()> {
    if cancel.is_cancelled() {
        Err(ImportError::Cancelled)
    } else {
        Ok(())
    }
}

fn account_error(error: AccountError) -> SettingsError {
    match error {
        AccountError::AlreadyExists | AccountError::StaleCapture => SettingsError::Conflict,
        AccountError::InvalidStoredData => SettingsError::Corrupt,
        AccountError::Storage => SettingsError::Unavailable,
        _ => SettingsError::Validation("The imported account selection is invalid."),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAccounts {
    schema: String,
    schema_version: u32,
    active_account_id: Option<String>,
    accounts: Vec<LegacyAccount>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAccount {
    account_id: String,
    kind: String,
    display_name: String,
    login_id: Option<String>,
    minecraft_profile_id: Option<String>,
    offline_uuid: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Clone)]
pub(super) struct PreparedAccounts {
    pub(super) offline: Vec<OfflineIdentityImport>,
    pub(super) microsoft: Vec<MicrosoftIdentityImport>,
    mapping: BTreeMap<String, String>,
    active_id: String,
    active_name: String,
    active_mode: ConfigLaunchAuthMode,
}

pub(super) fn prepare_accounts(
    inventory: &Inventory,
    settings: &PreparedSettingsImport,
) -> ImportResult<PreparedAccounts> {
    let prepared = convert_accounts(
        serde_json::from_slice(&inventory.record_bytes("profile/accounts.json")?)
            .map_err(|_| ImportError::InvalidData)?,
    )?;
    if prepared.active_mode != settings.config.launch_auth_mode
        || prepared.active_name != settings.config.username
    {
        return Err(ImportError::InvalidData);
    }
    Ok(prepared)
}

/// Shared by preview eligibility and both metadata and ordinary-payload import.
/// The predecessor stores login-derived IDs; only profile UUIDs become new
/// Microsoft identities. Login IDs are validated but never transferred.
pub(super) fn convert_accounts(value: serde_json::Value) -> ImportResult<PreparedAccounts> {
    let source: LegacyAccounts =
        serde_json::from_value(value).map_err(|_| ImportError::InvalidData)?;
    if source.schema != "axial.accounts"
        || source.schema_version != 1
        || source.accounts.is_empty()
        || source.accounts.len() > 256
    {
        return Err(ImportError::InvalidData);
    }
    let active = source.active_account_id.ok_or(ImportError::InvalidData)?;
    let mut offline = Vec::new();
    let mut microsoft = Vec::new();
    let mut mapping = BTreeMap::new();
    let mut destinations = HashSet::new();
    let mut selected = None;
    for account in source.accounts {
        let legacy_id = account.account_id.clone();
        let name = account.display_name.clone();
        let input = convert_account(account)?;
        let destination_id = input.destination_id()?;
        let mode = match input {
            ImportedIdentity::Offline(input) => {
                offline.push(input);
                ConfigLaunchAuthMode::Offline
            }
            ImportedIdentity::Microsoft(input) => {
                microsoft.push(input);
                ConfigLaunchAuthMode::Online
            }
        };
        if !destinations.insert(destination_id.clone())
            || mapping
                .insert(legacy_id.clone(), destination_id.clone())
                .is_some()
        {
            return Err(ImportError::InvalidData);
        }
        if legacy_id == active {
            selected = Some((destination_id, name, mode));
        }
    }
    let (active_id, active_name, active_mode) = selected.ok_or(ImportError::InvalidData)?;
    Ok(PreparedAccounts {
        offline,
        microsoft,
        mapping,
        active_id,
        active_name,
        active_mode,
    })
}

fn legacy_microsoft_id(value: &str) -> bool {
    value
        .strip_prefix("microsoft-")
        .is_some_and(legacy_login_id)
}

fn legacy_login_id(value: &str) -> bool {
    let Some((nanos, sequence)) = value
        .strip_prefix("msa-")
        .and_then(|value| value.split_once('-'))
    else {
        return false;
    };
    let hex = |value: &str, max| {
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    hex(nanos, 32) && hex(sequence, 16)
}

enum ImportedIdentity {
    Offline(OfflineIdentityImport),
    Microsoft(MicrosoftIdentityImport),
}

impl ImportedIdentity {
    fn destination_id(&self) -> ImportResult<String> {
        match self {
            Self::Offline(input) => Ok(input.account_id.clone()),
            Self::Microsoft(input) => input.account_id().map_err(|_| ImportError::InvalidData),
        }
    }
}

pub(super) fn destination_account_id(value: &serde_json::Value) -> ImportResult<String> {
    convert_account(serde_json::from_value(value.clone()).map_err(|_| ImportError::InvalidData)?)?
        .destination_id()
}

fn convert_account(account: LegacyAccount) -> ImportResult<ImportedIdentity> {
    match account.kind.as_str() {
        "offline" => {
            if account.login_id.is_some() || account.minecraft_profile_id.is_some() {
                return Err(ImportError::InvalidData);
            }
            let input = OfflineIdentityImport {
                account_id: account.account_id,
                display_name: account.display_name,
                offline_uuid: account.offline_uuid.ok_or(ImportError::InvalidData)?,
                created_at: account.created_at,
                updated_at: account.updated_at,
            };
            input.validate().map_err(|_| ImportError::InvalidData)?;
            Ok(ImportedIdentity::Offline(input))
        }
        "microsoft" => {
            if !legacy_microsoft_id(&account.account_id)
                || account.offline_uuid.is_some()
                || !account.login_id.as_deref().is_some_and(legacy_login_id)
            {
                return Err(ImportError::InvalidData);
            }
            let input = MicrosoftIdentityImport {
                profile_id: account
                    .minecraft_profile_id
                    .ok_or(ImportError::InvalidData)?,
                display_name: account.display_name,
                created_at: account.created_at,
                updated_at: account.updated_at,
            };
            input.validate().map_err(|_| ImportError::InvalidData)?;
            Ok(ImportedIdentity::Microsoft(input))
        }
        _ => Err(ImportError::InvalidData),
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
