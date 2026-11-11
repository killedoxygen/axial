//! The persisted copy of the last verified remote rules.

use super::signature::{RemoteRulesVerifier, RulesSignatureMetadata};
use crate::performance::model::{Manifest, RuleChannel, RuleSource, RulesValidation};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io;

pub(super) const MAX_BYTES: usize = 1024 * 1024;

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

impl RulesCacheSnapshot {
    pub(super) fn remote(manifest: Manifest, signature: RulesSignatureMetadata) -> Self {
        Self {
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            schema_version: manifest.schema_version,
            generated_at: manifest.generated_at.clone(),
            validation: RulesValidation::Valid,
            updated_at: Utc::now().to_rfc3339(),
            manifest,
            signature,
        }
    }

    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if encoded.len() > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "performance rules cache exceeds its size limit",
            ));
        }
        Ok(encoded)
    }

    /// Reopening re-authenticates the stored manifest under the current trust anchor.
    pub(super) fn decode_verified(bytes: &[u8], verifier: &RemoteRulesVerifier) -> Option<Self> {
        if bytes.len() > MAX_BYTES {
            return None;
        }
        let snapshot = serde_json::from_slice::<Self>(bytes).ok()?;
        let valid = snapshot.rule_source == RuleSource::Remote
            && snapshot.rule_channel == RuleChannel::Remote
            && snapshot.validation == RulesValidation::Valid
            && DateTime::parse_from_rfc3339(&snapshot.updated_at).is_ok()
            && snapshot.schema_version == snapshot.manifest.schema_version
            && snapshot.generated_at == snapshot.manifest.generated_at
            && verifier
                .verify_manifest(&snapshot.manifest, &snapshot.signature)
                .is_ok();
        valid.then_some(snapshot)
    }
}
