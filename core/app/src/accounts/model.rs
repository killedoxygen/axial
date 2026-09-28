//! Account identity and revision contracts. Credentials never enter these records.

use serde::{Deserialize, Deserializer, Serialize};

use super::microsoft::MinecraftProfile;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    pub fn parse(value: &str) -> Result<Self, AccountError> {
        if let Some(uuid) = value.strip_prefix("offline-") {
            if uuid.len() == 32
                && uuid
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Ok(Self(value.to_owned()));
            }
        } else if let Ok(uuid) = uuid::Uuid::parse_str(value) {
            return Ok(Self(uuid.hyphenated().to_string()));
        }
        Err(AccountError::InvalidInput("Account identity is invalid."))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Offline,
    Microsoft,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchAuthMode {
    #[default]
    Offline,
    Online,
}

impl AccountKind {
    pub fn launch_mode(self) -> LaunchAuthMode {
        match self {
            Self::Offline => LaunchAuthMode::Offline,
            Self::Microsoft => LaunchAuthMode::Online,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountRecord {
    pub account_id: AccountId,
    pub kind: AccountKind,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minecraft_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minecraft_profile: Option<MinecraftProfile>,
    pub account_revision: u64,
    pub profile_revision: u64,
    /// Zero for offline identities and imported Microsoft identities awaiting sign-in.
    pub credential_revision: u64,
    pub created_revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

impl AccountRecord {
    pub fn minecraft_uuid(&self) -> &str {
        match self.kind {
            AccountKind::Offline => self.offline_uuid.as_deref(),
            AccountKind::Microsoft => self.minecraft_profile_id.as_deref(),
        }
        .expect("validated persisted account has its Minecraft identity")
    }
}

/// All account and selection fields are read inside one metadata transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSnapshot {
    pub revision: u64,
    /// Revision of the entire account selection snapshot, including removals.
    pub selection_revision: u64,
    pub active_account_id: Option<AccountId>,
    pub launch_auth_mode: LaunchAuthMode,
    pub accounts: Vec<AccountRecord>,
}

impl AccountSnapshot {
    pub fn active_account(&self) -> Option<&AccountRecord> {
        self.active_account_id.as_ref().and_then(|id| {
            self.accounts
                .iter()
                .find(|account| &account.account_id == id)
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountPreconditions {
    pub expected_selection_revision: Option<u64>,
    pub expected_account_revision: Option<u64>,
}

/// The authentication owner supplies this only after the secure store acknowledges
/// the exact credential revision. No access or refresh token is persisted here.
#[derive(Clone, Debug)]
pub struct MicrosoftIdentity {
    pub login_id: String,
    pub profile_id: String,
    pub display_name: String,
    pub credential_revision: u64,
    pub profile: MinecraftProfile,
}

/// Retained identity only. Legacy sessions and credentials are never imported.
#[derive(Clone, Debug)]
pub struct MicrosoftIdentityImport {
    pub profile_id: String,
    pub display_name: String,
    pub created_at: String,
    pub updated_at: String,
}

impl MicrosoftIdentityImport {
    pub fn account_id(&self) -> Result<String, AccountError> {
        let id = uuid::Uuid::parse_str(&self.profile_id)
            .map_err(|_| AccountError::InvalidInput("Imported Microsoft identity is invalid."))?;
        if id.is_nil() {
            return Err(AccountError::InvalidInput(
                "Imported Microsoft identity is invalid.",
            ));
        }
        Ok(id.hyphenated().to_string())
    }

    pub fn validate(&self) -> Result<(), AccountError> {
        self.account_id()?;
        if !(1..=16).contains(&self.display_name.len())
            || !self
                .display_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || [&self.created_at, &self.updated_at]
                .into_iter()
                .any(|value| {
                    value.len() > 64 || chrono::DateTime::parse_from_rfc3339(value).is_err()
                })
        {
            return Err(AccountError::InvalidInput(
                "Imported Microsoft identity is invalid.",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct OfflineIdentityImport {
    pub account_id: String,
    pub display_name: String,
    pub offline_uuid: String,
    pub created_at: String,
    pub updated_at: String,
}

impl OfflineIdentityImport {
    pub fn validate(&self) -> Result<(), AccountError> {
        let name = validate_username(&self.display_name)?;
        let uuid = offline_uuid(&name);
        if self.display_name != name
            || self.offline_uuid != uuid
            || self.account_id != format!("offline-{uuid}")
            || [&self.created_at, &self.updated_at]
                .into_iter()
                .any(|value| {
                    value.len() > 64 || chrono::DateTime::parse_from_rfc3339(value).is_err()
                })
        {
            return Err(AccountError::InvalidInput(
                "Imported offline identity is invalid.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("{0}")]
    InvalidInput(&'static str),
    #[error("Account is not available.")]
    NotFound,
    #[error("Select an account before launching.")]
    NoSelection,
    #[error("Account changed. Refresh and try again.")]
    StaleCapture,
    #[error("An account with this identity already exists.")]
    AlreadyExists,
    #[error("This action requires an offline identity.")]
    NotOffline,
    #[error("This action requires a Microsoft account.")]
    NotMicrosoft,
    #[error("Account data is invalid. The existing data was preserved.")]
    InvalidStoredData,
    #[error("Could not save account changes. Check app data permissions and try again.")]
    Storage,
}

impl From<crate::storage::StorageError> for AccountError {
    fn from(error: crate::storage::StorageError) -> Self {
        tracing::warn!(%error, "account metadata operation failed");
        Self::Storage
    }
}

impl From<crate::storage::rusqlite::Error> for AccountError {
    fn from(error: crate::storage::rusqlite::Error) -> Self {
        crate::storage::StorageError::from(error).into()
    }
}

pub fn validate_username(raw: &str) -> Result<String, AccountError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(AccountError::InvalidInput("Enter a name."));
    }
    if value.len() < 3 {
        return Err(AccountError::InvalidInput("At least 3 characters."));
    }
    if value.len() > 16 {
        return Err(AccountError::InvalidInput("At most 16 characters."));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(AccountError::InvalidInput(
            "Letters, numbers, and underscores only.",
        ));
    }
    Ok(value.to_owned())
}

/// Minecraft's name UUID hashes the literal prefix without a UUID namespace.
pub fn offline_uuid(username: &str) -> String {
    let mut bytes = md5::compute(format!("OfflinePlayer:{username}").as_bytes()).0;
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).simple().to_string()
}

pub fn microsoft_account_id(profile_id: &str) -> Result<String, AccountError> {
    uuid::Uuid::parse_str(profile_id)
        .map(|id| id.hyphenated().to_string())
        .map_err(|_| AccountError::InvalidInput("Minecraft profile identity is invalid."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_microsoft_identity_is_non_nil_and_accepts_provider_names_not_offline_names() {
        let input = MicrosoftIdentityImport {
            profile_id: "12345678123442348234123456789ABC".into(),
            display_name: "A".into(),
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: "2024-01-02T00:00:00Z".into(),
        };
        input.validate().unwrap();
        assert_eq!(
            input.account_id().unwrap(),
            "12345678-1234-4234-8234-123456789abc"
        );
        for id in ["", "../old-login", "00000000-0000-0000-0000-000000000000"] {
            assert!(
                MicrosoftIdentityImport {
                    profile_id: id.into(),
                    ..input.clone()
                }
                .validate()
                .is_err()
            );
        }
        for name in ["", "not a player", "é", "abcdefghijklmnopq"] {
            assert!(
                MicrosoftIdentityImport {
                    display_name: name.into(),
                    ..input.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            MicrosoftIdentityImport {
                created_at: "yesterday".into(),
                ..input
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn name_uuid_matches_minecraft_and_preserves_case() {
        assert_eq!(offline_uuid("Notch"), "b50ad385829d3141a2167e7d7539ba7f");
        assert_eq!(offline_uuid("Steve"), "5627dd98e6be3c21b8a8e92344183641");
        assert_ne!(offline_uuid("Steve"), offline_uuid("steve"));
    }

    #[test]
    fn validates_names_and_opaque_account_ids() {
        assert_eq!(validate_username("  Player_1  ").unwrap(), "Player_1");
        for name in [
            "",
            "ab",
            "abcdefghijklmnopq",
            "Player Name",
            "Pláyer",
            "../Steve",
        ] {
            assert!(validate_username(name).is_err());
        }
        assert!(AccountId::parse("offline-5627dd98e6be3c21b8a8e92344183641").is_ok());
        assert!(AccountId::parse("../../profile").is_err());
    }
}
