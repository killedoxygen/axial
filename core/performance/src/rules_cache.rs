use crate::resolve::validate_manifest;
use crate::signature::{RemoteRulesVerifier, RulesSignatureMetadata};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::Manifest;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io;

pub const RULES_CACHE_MAX_BYTES: u64 = 1024 * 1024;
const MAX_CACHE_WARNING_CHARS: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesCacheSnapshot {
    pub rule_source: RuleSource,
    pub rule_channel: RuleChannel,
    pub schema_version: i32,
    pub generated_at: String,
    pub validation: RulesValidation,
    pub updated_at: String,
    pub manifest: Manifest,
    pub signature: RulesSignatureMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulesCacheState {
    Recorded,
    Invalid,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesCacheStatus {
    pub recorded: bool,
    pub state: RulesCacheState,
    pub updated_at: Option<String>,
    pub loaded_at: Option<String>,
    pub warning: Option<String>,
}

impl RulesCacheStatus {
    pub fn unavailable() -> Self {
        Self {
            recorded: false,
            state: RulesCacheState::Unavailable,
            updated_at: None,
            loaded_at: None,
            warning: None,
        }
    }

    pub fn from_snapshot(snapshot: &RulesCacheSnapshot, state: RulesCacheState) -> Self {
        Self {
            recorded: true,
            state,
            updated_at: Some(snapshot.updated_at.clone()),
            loaded_at: Some(Utc::now().to_rfc3339()),
            warning: None,
        }
    }
}

pub(crate) fn remote_rules_snapshot(
    manifest: &Manifest,
    signature: RulesSignatureMetadata,
) -> RulesCacheSnapshot {
    let now = Utc::now().to_rfc3339();
    remote_snapshot(manifest, signature, now)
}

impl RulesCacheSnapshot {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if encoded.len() as u64 > RULES_CACHE_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "performance rules cache exceeds its size limit",
            ));
        }
        Ok(encoded)
    }
}

fn remote_snapshot(
    manifest: &Manifest,
    signature: RulesSignatureMetadata,
    now: String,
) -> RulesCacheSnapshot {
    RulesCacheSnapshot {
        rule_source: RuleSource::Remote,
        rule_channel: RuleChannel::Remote,
        schema_version: manifest.schema_version,
        generated_at: manifest.generated_at.clone(),
        validation: RulesValidation::Valid,
        updated_at: now,
        manifest: manifest.clone(),
        signature,
    }
}

pub(crate) fn remote_snapshot_manifest(
    snapshot: &RulesCacheSnapshot,
    verifier: &RemoteRulesVerifier,
) -> Result<Manifest, String> {
    if snapshot.rule_source != RuleSource::Remote
        || snapshot.rule_channel != RuleChannel::Remote
        || snapshot.validation != RulesValidation::Valid
        || !valid_timestamp(&snapshot.updated_at)
    {
        return Err("Remote rules cache was invalid; using the built-in manifest.".to_string());
    }

    let manifest = snapshot.manifest.clone();
    let signature = &snapshot.signature;
    if snapshot.schema_version != manifest.schema_version
        || snapshot.generated_at != manifest.generated_at
        || validate_manifest(&manifest).is_err()
    {
        return Err("Remote rules cache was invalid; using the built-in manifest.".to_string());
    }
    verifier
        .verify_manifest(&manifest, signature)
        .map_err(|error| {
            format!("Remote rules cache signature rejected: {error}; using the built-in manifest.")
        })?;
    Ok(manifest)
}

fn valid_timestamp(value: &str) -> bool {
    DateTime::parse_from_rfc3339(value).is_ok()
}

pub fn bounded_warning(value: String) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CACHE_WARNING_CHARS {
        return compact;
    }

    let mut truncated = compact
        .chars()
        .take(MAX_CACHE_WARNING_CHARS.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::{RulesCacheSnapshot, RulesCacheState, remote_rules_snapshot};
    use crate::install::PerformanceManager;
    use crate::resolve::builtin_manifest;
    use crate::signature::RulesSignatureMetadata;
    use crate::status::{PerformanceRulesStatus, RuleChannel, RuleSource, RulesValidation};
    use ed25519_dalek::{Signer, SigningKey};

    fn load(bytes: &[u8], public_key: Option<String>) -> PerformanceRulesStatus {
        PerformanceManager::from_cached_rules(
            Some(bytes),
            Some("https://rules.example.invalid/manifest.json".to_string()),
            public_key,
        )
        .expect("performance manager")
        .rules_status()
    }

    fn assert_rejected(status: &PerformanceRulesStatus) {
        assert_eq!(status.rule_source, RuleSource::BuiltIn);
        assert_eq!(status.rule_channel, RuleChannel::Bundled);
        assert_eq!(status.rules_cache.state, RulesCacheState::Invalid);
        assert!(!status.rules_cache.recorded);
        assert!(status.rules_cache.warning.is_some());
    }

    #[test]
    fn valid_remote_cache_is_loaded_as_active_rules() {
        let mut remote = builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-05-30T10:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        let snapshot = remote_rules_snapshot(&remote, signature);

        let status = load(
            &snapshot.encode().expect("encode remote cache"),
            Some(public_key),
        );

        assert_eq!(status.rule_source, RuleSource::Remote);
        assert_eq!(status.rule_channel, RuleChannel::Remote);
        assert_eq!(status.generated_at, remote.generated_at);
        assert_eq!(status.validation, RulesValidation::Valid);
        assert_eq!(status.last_refresh_at, Some(snapshot.updated_at));
        assert!(status.rules_cache.recorded);
        assert_eq!(status.rules_cache.state, RulesCacheState::Recorded);
        assert!(status.rules_cache.warning.is_none());
    }

    #[test]
    fn malformed_or_invalid_remote_cache_falls_back_to_builtin_with_warning() {
        let mut remote = builtin_manifest().expect("builtin manifest");
        remote.schema_version = 99;
        let (public_key, signature) = signed_metadata(&remote);
        let invalid = serde_json::to_vec(&RulesCacheSnapshot {
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            schema_version: remote.schema_version,
            generated_at: remote.generated_at.clone(),
            validation: RulesValidation::Valid,
            updated_at: "2026-05-30T10:00:00Z".to_string(),
            manifest: remote,
            signature,
        })
        .expect("serialize invalid remote cache");

        assert_rejected(&load(b"{not json", Some(public_key.clone())));
        assert_rejected(&load(&invalid, Some(public_key)));
    }

    #[test]
    fn remote_cache_without_a_valid_signature_falls_back_to_builtin() {
        let mut remote = builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-05-30T10:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        let unsigned = serde_json::to_vec(&RulesCacheSnapshot {
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            schema_version: remote.schema_version,
            generated_at: remote.generated_at.clone(),
            validation: RulesValidation::Valid,
            updated_at: "2026-05-30T10:00:00Z".to_string(),
            manifest: remote.clone(),
            signature: RulesSignatureMetadata {
                signature: String::new(),
                key_id: None,
            },
        })
        .expect("serialize unsigned remote cache");
        let absent = serde_json::to_vec(&serde_json::json!({
            "rule_source": "remote",
            "rule_channel": "remote",
            "schema_version": remote.schema_version,
            "generated_at": remote.generated_at,
            "validation": "valid",
            "updated_at": "2026-05-30T10:00:00Z",
            "manifest": remote
        }))
        .expect("serialize remote cache missing signature field");
        let signed = remote_rules_snapshot(&remote, signature)
            .encode()
            .expect("encode signed cache");

        assert_rejected(&load(&unsigned, Some(public_key.clone())));
        assert_rejected(&load(&absent, Some(public_key)));
        assert_rejected(&load(&signed, None));
    }

    fn signed_metadata(manifest: &crate::types::Manifest) -> (String, RulesSignatureMetadata) {
        let signing_key = SigningKey::from_bytes(&[9_u8; 32]);
        let payload = crate::signature::canonical_manifest_payload(manifest).expect("payload");
        let signature = signing_key.sign(&payload);
        (
            hex::encode(signing_key.verifying_key().to_bytes()),
            RulesSignatureMetadata {
                signature: hex::encode(signature.to_bytes()),
                key_id: Some("cache-test-key".to_string()),
            },
        )
    }
}
