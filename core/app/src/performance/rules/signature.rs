//! Ed25519 authentication of remote rules manifests.

use crate::performance::model::Manifest;
use crate::performance::resolve::{ResolveError, validate_manifest};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub(super) const SIGNATURE_HEADER: &str = "x-axial-rules-signature-ed25519";
pub(super) const KEY_ID_HEADER: &str = "x-axial-rules-key-id";

const PUBLIC_KEY_BYTES: usize = 32;
const SIGNATURE_BYTES: usize = 64;
const MAX_KEY_ID_CHARS: usize = 64;

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

/// The configured trust anchor. Absent or malformed keys refuse every remote manifest.
#[derive(Debug, Clone)]
pub(super) enum RemoteRulesVerifier {
    MissingPublicKey,
    InvalidPublicKey,
    Ready(VerifyingKey),
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
    #[error("remote rules signature key id is invalid")]
    InvalidKeyId,
    #[error("failed to serialize remote rules signature payload: {0}")]
    Payload(#[from] serde_json::Error),
    #[error("remote performance manifest failed validation: {0}")]
    Validation(#[from] ResolveError),
}

impl RemoteRulesVerifier {
    pub(super) fn from_public_key_hex(value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None => Self::MissingPublicKey,
            Some(value) => parse_public_key(value).map_or(Self::InvalidPublicKey, Self::Ready),
        }
    }

    pub(super) fn public_key(&self) -> Result<&VerifyingKey, RulesSignatureError> {
        match self {
            Self::MissingPublicKey => Err(RulesSignatureError::MissingPublicKey),
            Self::InvalidPublicKey => Err(RulesSignatureError::InvalidPublicKey),
            Self::Ready(public_key) => Ok(public_key),
        }
    }

    pub(super) fn verify_manifest(
        &self,
        manifest: &Manifest,
        metadata: &RulesSignatureMetadata,
    ) -> Result<(), RulesSignatureError> {
        let public_key = self.public_key()?;
        validate_manifest(manifest)?;
        if metadata
            .key_id
            .as_deref()
            .is_some_and(|key_id| !valid_key_id(key_id))
        {
            return Err(RulesSignatureError::InvalidKeyId);
        }
        let payload = canonical_manifest_payload(manifest)?;
        let signature = parse_signature(&metadata.signature)?;
        public_key
            .verify_strict(&payload, &signature)
            .map_err(|_| RulesSignatureError::VerificationFailed)
    }
}

fn valid_key_id(key_id: &str) -> bool {
    !key_id.is_empty()
        && key_id.trim() == key_id
        && key_id.chars().count() <= MAX_KEY_ID_CHARS
        && !key_id.chars().any(char::is_control)
}

pub fn canonical_manifest_payload(manifest: &Manifest) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(manifest)
}

pub(super) fn signature_metadata_from_header(
    signature: Option<&str>,
    key_id: Option<&str>,
) -> Result<RulesSignatureMetadata, RulesSignatureError> {
    let Some(signature) = signature.map(str::trim).filter(|value| !value.is_empty()) else {
        return Err(RulesSignatureError::MissingSignature);
    };
    parse_signature(signature)?;
    if key_id.is_some_and(|key_id| !valid_key_id(key_id)) {
        return Err(RulesSignatureError::InvalidKeyId);
    }
    Ok(RulesSignatureMetadata {
        signature: signature.to_ascii_lowercase(),
        key_id: key_id.map(str::to_string),
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
    use super::{
        RemoteRulesVerifier, RulesSignatureError, RulesSignatureMetadata,
        signature_metadata_from_header,
    };
    use crate::performance::resolve::builtin_manifest;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn signature_verifies_only_the_signed_manifest_payload() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let (public_key, metadata) = signed_metadata(&manifest);
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(&public_key));
        verifier
            .verify_manifest(&manifest, &metadata)
            .expect("signature should verify");

        let mut changed = manifest;
        changed.generated_at = "2026-05-30T15:00:00Z".to_string();
        assert!(matches!(
            verifier.verify_manifest(&changed, &metadata),
            Err(RulesSignatureError::VerificationFailed)
        ));
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

    #[test]
    fn signature_header_rejects_padded_or_oversized_key_id() {
        let signature = "00".repeat(64);

        assert!(matches!(
            signature_metadata_from_header(Some(&signature), Some(" padded ")),
            Err(RulesSignatureError::InvalidKeyId)
        ));
        assert!(matches!(
            signature_metadata_from_header(Some(&signature), Some(&"x".repeat(65))),
            Err(RulesSignatureError::InvalidKeyId)
        ));
    }

    fn signed_metadata(
        manifest: &crate::performance::model::Manifest,
    ) -> (String, RulesSignatureMetadata) {
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
