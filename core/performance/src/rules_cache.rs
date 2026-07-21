use crate::resolve::validate_manifest;
use crate::signature::{RemoteRulesVerifier, RulesSignatureMetadata};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::Manifest;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const RULES_CACHE_FILE: &str = "rules-cache.json";
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

    fn invalid(loaded_at: String, warning: impl Into<String>) -> Self {
        Self {
            recorded: false,
            state: RulesCacheState::Invalid,
            updated_at: None,
            loaded_at: Some(loaded_at),
            warning: Some(bounded_warning(warning.into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedRulesCache {
    pub manifest: Manifest,
    pub rule_source: RuleSource,
    pub rule_channel: RuleChannel,
    pub validation: RulesValidation,
    pub last_refresh_at: Option<String>,
    pub status: RulesCacheStatus,
    pub mutation_allowed: bool,
}

pub fn rules_cache_path(performance_dir: &Path) -> PathBuf {
    performance_dir.join(RULES_CACHE_FILE)
}

pub(crate) fn load_active_rules_cache(
    performance_dir: &Path,
    builtin_manifest: &Manifest,
    remote_enabled: bool,
    verifier: &RemoteRulesVerifier,
) -> LoadedRulesCache {
    if !remote_enabled {
        let (status, mutation_allowed) =
            load_rules_cache_status(performance_dir, builtin_manifest);
        return LoadedRulesCache {
            manifest: builtin_manifest.clone(),
            rule_source: RuleSource::BuiltIn,
            rule_channel: RuleChannel::Bundled,
            validation: RulesValidation::Valid,
            last_refresh_at: None,
            status,
            mutation_allowed,
        };
    }

    if let Some(warning) = verifier.acceptance_warning() {
        let (mut status, mutation_allowed) =
            load_rules_cache_status(performance_dir, builtin_manifest);
        status.warning = Some(bounded_warning(warning));
        return LoadedRulesCache {
            manifest: builtin_manifest.clone(),
            rule_source: RuleSource::BuiltIn,
            rule_channel: RuleChannel::Bundled,
            validation: RulesValidation::Valid,
            last_refresh_at: None,
            status,
            mutation_allowed,
        };
    }

    let path = rules_cache_path(performance_dir);
    let loaded_at = Utc::now().to_rfc3339();
    match read_snapshot(&path) {
        Ok(Some(snapshot)) => match remote_snapshot_manifest(&snapshot, verifier) {
            Ok(manifest) => {
                let status = RulesCacheStatus::from_snapshot(&snapshot, RulesCacheState::Recorded);
                LoadedRulesCache {
                    manifest,
                    rule_source: RuleSource::Remote,
                    rule_channel: RuleChannel::Remote,
                    validation: RulesValidation::Valid,
                    last_refresh_at: Some(snapshot.updated_at.clone()),
                    status,
                    mutation_allowed: true,
                }
            }
            Err(warning) => builtin_with_status(
                builtin_manifest,
                RulesCacheStatus::invalid(loaded_at, warning),
            ),
        },
        Ok(None) => LoadedRulesCache {
            manifest: builtin_manifest.clone(),
            rule_source: RuleSource::BuiltIn,
            rule_channel: RuleChannel::Bundled,
            validation: RulesValidation::Valid,
            last_refresh_at: None,
            status: RulesCacheStatus::unavailable(),
            mutation_allowed: true,
        },
        Err(_) => builtin_with_status(
            builtin_manifest,
            RulesCacheStatus::invalid(
                loaded_at,
                "Remote rules cache was invalid; using the built-in manifest.",
            ),
        ),
    }
}

fn load_rules_cache_status(performance_dir: &Path, _manifest: &Manifest) -> (RulesCacheStatus, bool) {
    let path = rules_cache_path(performance_dir);
    let loaded_at = Utc::now().to_rfc3339();

    match read_snapshot(&path) {
        Ok(Some(_)) | Err(_) => (
            RulesCacheStatus::invalid(
                loaded_at,
                "Rules cache is invalid; using the built-in manifest.",
            ),
            false,
        ),
        Ok(None) => (RulesCacheStatus::unavailable(), true),
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

fn remote_snapshot_manifest(
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

fn builtin_with_status(manifest: &Manifest, status: RulesCacheStatus) -> LoadedRulesCache {
    LoadedRulesCache {
        manifest: manifest.clone(),
        rule_source: RuleSource::BuiltIn,
        rule_channel: RuleChannel::Bundled,
        validation: RulesValidation::Valid,
        last_refresh_at: None,
        status,
        mutation_allowed: false,
    }
}

fn read_snapshot(path: &Path) -> io::Result<Option<RulesCacheSnapshot>> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "performance rules cache has no parent directory",
        )
    })?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "performance rules cache parent is not a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "performance rules cache is not a regular file",
        ));
    }
    if metadata.len() > RULES_CACHE_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "performance rules cache exceeds its size limit",
        ));
    }
    let mut file = fs::File::open(path)?;
    let mut data = Vec::new();
    file.by_ref()
        .take(RULES_CACHE_MAX_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > RULES_CACHE_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "performance rules cache exceeds its size limit",
        ));
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
    use super::{
        RulesCacheSnapshot, RulesCacheState, RulesCacheStatus, load_active_rules_cache,
        remote_rules_snapshot, rules_cache_path,
    };
    use crate::resolve::builtin_manifest;
    use crate::signature::{RemoteRulesVerifier, RulesSignatureMetadata};
    use crate::status::{RuleChannel, RuleSource, RulesValidation};
    use ed25519_dalek::{Signer, SigningKey};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn missing_cache_is_not_materialized() {
        let root = test_root("missing");
        let manifest = builtin_manifest().expect("builtin manifest");

        let loaded =
            load_active_rules_cache(&root, &manifest, false, &RemoteRulesVerifier::disabled());
        let status = loaded.status;

        assert!(!status.recorded);
        assert_eq!(status.state, RulesCacheState::Unavailable);
        assert!(status.updated_at.is_none());
        assert!(status.loaded_at.is_none());
        assert!(status.warning.is_none());
        assert!(!rules_cache_path(&root).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_cache_is_reported_without_repairing_file() {
        let root = test_root("invalid");
        let manifest = builtin_manifest().expect("builtin manifest");
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(&path, "{not json").expect("write invalid cache");

        let status =
            load_active_rules_cache(&root, &manifest, false, &RemoteRulesVerifier::disabled())
                .status;

        assert!(!status.recorded);
        assert_eq!(status.state, RulesCacheState::Invalid);
        assert_eq!(status.updated_at, None);
        assert!(status.loaded_at.is_some());
        assert!(status.warning.is_some());
        assert_eq!(
            fs::read_to_string(&path).expect("read invalid cache"),
            "{not json"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn valid_remote_cache_is_loaded_as_active_rules() {
        let root = test_root("valid-remote");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.generated_at = "2026-05-30T10:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(public_key));
        let snapshot = remote_rules_snapshot(&remote, signature);
        let remote_status = RulesCacheStatus::from_snapshot(&snapshot, RulesCacheState::Recorded);
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(&path, snapshot.encode().expect("encode remote cache"))
            .expect("write remote cache");

        let loaded = load_active_rules_cache(&root, &builtin, true, &verifier);

        assert_eq!(loaded.rule_source, RuleSource::Remote);
        assert_eq!(loaded.rule_channel, RuleChannel::Remote);
        assert_eq!(loaded.manifest.generated_at, remote.generated_at);
        assert_eq!(loaded.validation, RulesValidation::Valid);
        assert_eq!(loaded.last_refresh_at, remote_status.updated_at);
        assert!(loaded.status.recorded);
        assert_eq!(loaded.status.state, RulesCacheState::Recorded);
        assert!(loaded.status.warning.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_remote_cache_falls_back_to_builtin_with_warning() {
        let root = test_root("invalid-remote");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.schema_version = 99;
        let (public_key, signature) = signed_metadata(&remote);
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(public_key));
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(
            &path,
            serde_json::to_vec(&RulesCacheSnapshot {
                rule_source: RuleSource::Remote,
                rule_channel: RuleChannel::Remote,
                schema_version: remote.schema_version,
                generated_at: remote.generated_at.clone(),
                validation: RulesValidation::Valid,
                updated_at: "2026-05-30T10:00:00Z".to_string(),
                manifest: remote,
                signature,
            })
            .expect("serialize invalid remote cache"),
        )
        .expect("write invalid remote cache");

        let loaded = load_active_rules_cache(&root, &builtin, true, &verifier);

        assert_eq!(loaded.rule_source, RuleSource::BuiltIn);
        assert_eq!(loaded.rule_channel, RuleChannel::Bundled);
        assert_eq!(loaded.manifest.generated_at, builtin.generated_at);
        assert_eq!(loaded.status.state, RulesCacheState::Invalid);
        assert!(!loaded.status.recorded);
        assert!(
            loaded
                .status
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("Remote rules cache was invalid"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn previous_manifest_schema_cache_is_not_accepted() {
        let root = test_root("previous-manifest-schema");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.schema_version = crate::PERFORMANCE_MANIFEST_SCHEMA_VERSION - 1;
        remote.generated_at = "2026-05-30T10:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(public_key));
        let snapshot = remote_rules_snapshot(&remote, signature);
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(
            &path,
            snapshot.encode().expect("encode previous schema cache"),
        )
        .expect("write previous schema cache");

        let loaded = load_active_rules_cache(&root, &builtin, true, &verifier);

        assert_eq!(loaded.rule_source, RuleSource::BuiltIn);
        assert_eq!(loaded.status.state, RulesCacheState::Invalid);
        assert!(!loaded.status.recorded);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_cache_with_null_signature_falls_back_to_builtin_with_warning() {
        let root = test_root("null-signature-remote");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.generated_at = "2026-05-30T10:00:00Z".to_string();
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(signed_metadata(&remote).0));
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(
            &path,
            serde_json::to_vec(&RulesCacheSnapshot {
                rule_source: RuleSource::Remote,
                rule_channel: RuleChannel::Remote,
                schema_version: remote.schema_version,
                generated_at: remote.generated_at.clone(),
                validation: RulesValidation::Valid,
                updated_at: "2026-05-30T10:00:00Z".to_string(),
                manifest: remote,
                signature: RulesSignatureMetadata {
                    signature: String::new(),
                    key_id: None,
                },
            })
            .expect("serialize unsigned remote cache"),
        )
        .expect("write unsigned remote cache");

        let loaded = load_active_rules_cache(&root, &builtin, true, &verifier);

        assert_eq!(loaded.rule_source, RuleSource::BuiltIn);
        assert!(
            loaded
                .status
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("signature rejected"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_cache_with_absent_signature_is_invalid_current_schema() {
        let root = test_root("absent-signature-remote");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.generated_at = "2026-05-30T10:00:00Z".to_string();
        let verifier = RemoteRulesVerifier::from_public_key_hex(Some(signed_metadata(&remote).0));
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "rule_source": "remote",
                "rule_channel": "remote",
                "schema_version": remote.schema_version,
                "generated_at": remote.generated_at,
                "validation": "valid",
                "updated_at": "2026-05-30T10:00:00Z",
                "loaded_at": "2026-05-30T10:00:00Z",
                "manifest": remote
            }))
            .expect("serialize remote cache missing signature field"),
        )
        .expect("write remote cache missing signature field");

        let loaded = load_active_rules_cache(&root, &builtin, true, &verifier);

        assert_eq!(loaded.rule_source, RuleSource::BuiltIn);
        assert!(
            loaded
                .status
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("Remote rules cache was invalid"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn configured_remote_without_public_key_uses_builtin_with_warning() {
        let root = test_root("missing-public-key");
        let builtin = builtin_manifest().expect("builtin manifest");
        let verifier = RemoteRulesVerifier::from_public_key_hex(None);

        let loaded = load_active_rules_cache(&root, &builtin, true, &verifier);

        assert_eq!(loaded.rule_source, RuleSource::BuiltIn);
        assert!(!loaded.status.recorded);
        assert!(
            loaded
                .status
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("public key is not configured"))
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "axial-performance-rules-cache-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
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
