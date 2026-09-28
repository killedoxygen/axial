//! Secret values are private application inputs, never a wire or metadata schema.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::credential_store::CredentialError;

pub(super) const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 24 * 1024;

/// A provider credential bundle. Deliberately does not implement `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    microsoft_access_token: String,
    microsoft_refresh_token: Option<String>,
    microsoft_expires_at: u64,
    minecraft_access_token: String,
    minecraft_expires_at: u64,
}

impl Credentials {
    pub fn new(
        microsoft_access_token: String,
        microsoft_refresh_token: Option<String>,
        microsoft_expires_at: u64,
        minecraft_access_token: String,
        minecraft_expires_at: u64,
    ) -> Result<Self, CredentialError> {
        validate_token(&microsoft_access_token)?;
        if let Some(token) = &microsoft_refresh_token {
            validate_token(token)?;
        }
        validate_token(&minecraft_access_token)?;
        if microsoft_expires_at == 0 || minecraft_expires_at == 0 {
            return Err(CredentialError::Malformed);
        }
        Ok(Self {
            microsoft_access_token,
            microsoft_refresh_token,
            microsoft_expires_at,
            minecraft_access_token,
            minecraft_expires_at,
        })
    }

    pub(crate) fn microsoft_access_token(&self) -> &str {
        &self.microsoft_access_token
    }

    pub(crate) fn microsoft_refresh_token(&self) -> Option<&str> {
        self.microsoft_refresh_token.as_deref()
    }

    pub fn microsoft_expires_at(&self) -> u64 {
        self.microsoft_expires_at
    }

    pub(crate) fn minecraft_access_token(&self) -> &str {
        &self.minecraft_access_token
    }

    pub fn minecraft_expires_at(&self) -> u64 {
        self.minecraft_expires_at
    }

    pub(super) fn encode_for_keyring(&self) -> Result<Vec<u8>, CredentialError> {
        let encoded = serde_json::to_vec(&KeyringCredentials {
            schema: 1,
            microsoft_access_token: self.microsoft_access_token.clone(),
            microsoft_refresh_token: self.microsoft_refresh_token.clone(),
            microsoft_expires_at: self.microsoft_expires_at,
            minecraft_access_token: self.minecraft_access_token.clone(),
            minecraft_expires_at: self.minecraft_expires_at,
        })
        .map_err(|_| CredentialError::Malformed)?;
        if encoded.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::Malformed);
        }
        Ok(encoded)
    }

    pub(super) fn decode_from_keyring(bytes: &[u8]) -> Result<Self, CredentialError> {
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::Malformed);
        }
        let stored: KeyringCredentials =
            serde_json::from_slice(bytes).map_err(|_| CredentialError::Malformed)?;
        if stored.schema != 1 {
            return Err(CredentialError::Malformed);
        }
        Self::new(
            stored.microsoft_access_token,
            stored.microsoft_refresh_token,
            stored.microsoft_expires_at,
            stored.minecraft_access_token,
            stored.minecraft_expires_at,
        )
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("tokens", &"[redacted]")
            .field("microsoft_expires_at", &self.microsoft_expires_at)
            .field("minecraft_expires_at", &self.minecraft_expires_at)
            .finish()
    }
}

// This encoding is reachable only through the OS-keyring adapter. It must never
// be added to public exports, metadata, profile export, logs, or HTTP responses.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyringCredentials {
    schema: u8,
    microsoft_access_token: String,
    microsoft_refresh_token: Option<String>,
    microsoft_expires_at: u64,
    minecraft_access_token: String,
    minecraft_expires_at: u64,
}

fn validate_token(token: &str) -> Result<(), CredentialError> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || token
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(CredentialError::Malformed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_reject_invalid_input_and_redact_debug() {
        assert!(Credentials::new(" ".into(), None, 1, "game".into(), 1).is_err());
        assert!(Credentials::new("msa".into(), Some("".into()), 1, "game".into(), 1).is_err());
        assert!(Credentials::new("msa".into(), None, 0, "game".into(), 1).is_err());
        let credentials = Credentials::new(
            "secret-access".into(),
            Some("secret-refresh".into()),
            1,
            "secret-game".into(),
            2,
        )
        .unwrap();
        assert!(!format!("{credentials:?}").contains("secret-"));
        let encoded = credentials.encode_for_keyring().unwrap();
        assert_eq!(
            Credentials::decode_from_keyring(&encoded).unwrap(),
            credentials
        );
    }
}
