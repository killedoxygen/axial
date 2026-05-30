use crate::resolve::validate_manifest;
use crate::types::Manifest;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const PERFORMANCE_RULES_PUBLIC_KEY_ENV: &str = "CROOPOR_PERFORMANCE_RULES_ED25519_PUBLIC_KEY";
pub const RULES_SIGNATURE_HEADER: &str = "x-croopor-rules-signature-ed25519";
pub const RULES_KEY_ID_HEADER: &str = "x-croopor-rules-key-id";

const PUBLIC_KEY_BYTES: usize = 32;
const SIGNATURE_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesSignatureMetadata {
    pub signature: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub key_id: Option<String>,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone)]
pub enum RemoteRulesVerifier {
    Disabled,
    MissingPublicKey,
    InvalidPublicKey { warning: String },
    Ready { public_key: VerifyingKey },
}

#[derive(Debug, Error)]
pub enum RulesSignatureError {
    #[error("remote rules public key is not configured")]
    MissingPublicKey,
    #[error("remote rules public key must be hex-encoded 32 bytes")]
    InvalidPublicKey,
    #[error("remote rules signature header is missing")]
    MissingSignature,
    #[error("remote rules signature must be hex-encoded 64 bytes")]
    InvalidSignatureEncoding,
    #[error("remote rules signature verification failed")]
    VerificationFailed,
    #[error("failed to serialize remote rules signature payload: {0}")]
    Payload(#[from] serde_json::Error),
    #[error("remote performance manifest failed validation: {0}")]
    Validation(#[from] crate::resolve::ResolveError),
}

impl RemoteRulesVerifier {
    pub fn disabled() -> Self {
        Self::Disabled
    }

    pub fn from_public_key_hex(value: Option<String>) -> Self {
        let Some(value) = value.map(|value| value.trim().to_string()) else {
            return Self::MissingPublicKey;
        };
        if value.is_empty() {
            return Self::MissingPublicKey;
        }

        match parse_public_key(&value) {
            Ok(public_key) => Self::Ready { public_key },
            Err(error) => Self::InvalidPublicKey {
                warning: error.to_string(),
            },
        }
    }

    pub fn acceptance_warning(&self) -> Option<String> {
        match self {
            Self::Disabled | Self::Ready { .. } => None,
            Self::MissingPublicKey => Some(
                "Remote rules public key is not configured; using the built-in manifest."
                    .to_string(),
            ),
            Self::InvalidPublicKey { warning } => {
                Some(format!("{warning}; using the built-in manifest."))
            }
        }
    }

    pub fn verify_manifest(
        &self,
        manifest: &Manifest,
        metadata: &RulesSignatureMetadata,
    ) -> Result<(), RulesSignatureError> {
        let Self::Ready { public_key } = self else {
            return Err(match self {
                Self::Disabled | Self::MissingPublicKey => RulesSignatureError::MissingPublicKey,
                Self::InvalidPublicKey { .. } => RulesSignatureError::InvalidPublicKey,
                Self::Ready { .. } => unreachable!(),
            });
        };

        validate_manifest(manifest)?;
        let payload = canonical_manifest_payload(manifest)?;
        let signature = parse_signature(&metadata.signature)?;
        public_key
            .verify_strict(&payload, &signature)
            .map_err(|_| RulesSignatureError::VerificationFailed)
    }
}

pub fn configured_remote_rules_verifier(remote_enabled: bool) -> RemoteRulesVerifier {
    if !remote_enabled {
        return RemoteRulesVerifier::disabled();
    }
    RemoteRulesVerifier::from_public_key_hex(std::env::var(PERFORMANCE_RULES_PUBLIC_KEY_ENV).ok())
}

pub fn canonical_manifest_payload(manifest: &Manifest) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(manifest)
}

pub fn signature_metadata_from_header(
    signature: Option<&str>,
    key_id: Option<&str>,
) -> Result<RulesSignatureMetadata, RulesSignatureError> {
    let Some(signature) = signature.map(str::trim).filter(|value| !value.is_empty()) else {
        return Err(RulesSignatureError::MissingSignature);
    };
    parse_signature(signature)?;
    Ok(RulesSignatureMetadata {
        signature: signature.to_ascii_lowercase(),
        key_id: key_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(64).collect()),
    })
}

fn parse_public_key(value: &str) -> Result<VerifyingKey, RulesSignatureError> {
    let bytes = hex::decode(value).map_err(|_| RulesSignatureError::InvalidPublicKey)?;
    let bytes: [u8; PUBLIC_KEY_BYTES] = bytes
        .try_into()
        .map_err(|_| RulesSignatureError::InvalidPublicKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| RulesSignatureError::InvalidPublicKey)
}

fn parse_signature(value: &str) -> Result<Signature, RulesSignatureError> {
    let bytes = hex::decode(value).map_err(|_| RulesSignatureError::InvalidSignatureEncoding)?;
    let bytes: [u8; SIGNATURE_BYTES] = bytes
        .try_into()
        .map_err(|_| RulesSignatureError::InvalidSignatureEncoding)?;
    Ok(Signature::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::{RemoteRulesVerifier, RulesSignatureError, RulesSignatureMetadata};
    use crate::resolve::builtin_manifest;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn verifies_current_schema_manifest_payload() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let (public_key, metadata) = signed_metadata(&manifest);
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(public_key));

        verifier
            .verify_manifest(&manifest, &metadata)
            .expect("signature should verify");
    }

    #[test]
    fn rejects_signature_for_different_payload() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let (public_key, metadata) = signed_metadata(&manifest);
        let mut changed = manifest.clone();
        changed.generated_at = "2026-05-30T15:00:00Z".to_string();
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(public_key));

        let error = verifier
            .verify_manifest(&changed, &metadata)
            .expect_err("signature should reject changed manifest");

        assert!(matches!(error, RulesSignatureError::VerificationFailed));
    }

    #[test]
    fn signature_metadata_rejects_unknown_fields() {
        let error = serde_json::from_value::<RulesSignatureMetadata>(serde_json::json!({
            "signature": "00".repeat(64),
            "key_id": null,
            "extra": "nope"
        }))
        .expect_err("unknown metadata fields should be invalid");

        assert!(error.to_string().contains("unknown field"));
    }

    fn signed_metadata(manifest: &crate::types::Manifest) -> (String, RulesSignatureMetadata) {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let payload = super::canonical_manifest_payload(manifest).expect("payload");
        let signature = signing_key.sign(&payload);
        (
            hex::encode(signing_key.verifying_key().to_bytes()),
            RulesSignatureMetadata {
                signature: hex::encode(signature.to_bytes()),
                key_id: Some("test-key".to_string()),
            },
        )
    }
}
