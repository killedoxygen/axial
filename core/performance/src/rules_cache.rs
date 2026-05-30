use crate::resolve::validate_manifest;
use crate::signature::{RemoteRulesVerifier, RulesSignatureMetadata};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::Manifest;
use chrono::Utc;
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const RULES_CACHE_FILE: &str = "rules-cache.json";
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
    pub loaded_at: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub manifest: Option<Manifest>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub signature: Option<RulesSignatureMetadata>,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
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

    fn from_snapshot(snapshot: &RulesCacheSnapshot, state: RulesCacheState) -> Self {
        Self {
            recorded: true,
            state,
            updated_at: Some(snapshot.updated_at.clone()),
            loaded_at: Some(snapshot.loaded_at.clone()),
            warning: None,
        }
    }

    fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(bounded_warning(warning.into()));
        self
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

    fn unavailable_with_warning(loaded_at: String, warning: impl Into<String>) -> Self {
        Self {
            recorded: false,
            state: RulesCacheState::Unavailable,
            updated_at: None,
            loaded_at: Some(loaded_at),
            warning: Some(bounded_warning(warning.into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRulesCache {
    pub manifest: Manifest,
    pub rule_source: RuleSource,
    pub rule_channel: RuleChannel,
    pub validation: RulesValidation,
    pub last_refresh_at: Option<String>,
    pub status: RulesCacheStatus,
}

pub fn rules_cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("performance").join(RULES_CACHE_FILE)
}

pub fn load_active_rules_cache(
    config_dir: &Path,
    builtin_manifest: &Manifest,
    remote_enabled: bool,
    verifier: &RemoteRulesVerifier,
) -> LoadedRulesCache {
    if !remote_enabled {
        let status = load_or_create_rules_cache(config_dir, builtin_manifest);
        return LoadedRulesCache {
            manifest: builtin_manifest.clone(),
            rule_source: RuleSource::BuiltIn,
            rule_channel: RuleChannel::Bundled,
            validation: RulesValidation::Valid,
            last_refresh_at: None,
            status,
        };
    }

    if let Some(warning) = verifier.acceptance_warning() {
        let mut status = load_or_create_rules_cache(config_dir, builtin_manifest);
        status.warning = Some(bounded_warning(warning));
        return LoadedRulesCache {
            manifest: builtin_manifest.clone(),
            rule_source: RuleSource::BuiltIn,
            rule_channel: RuleChannel::Bundled,
            validation: RulesValidation::Valid,
            last_refresh_at: None,
            status,
        };
    }

    let path = rules_cache_path(config_dir);
    let loaded_at = Utc::now().to_rfc3339();
    match fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<RulesCacheSnapshot>(&data) {
            Ok(mut snapshot) => {
                if snapshot_matches_manifest(&snapshot, builtin_manifest) {
                    snapshot.loaded_at = loaded_at;
                    return LoadedRulesCache {
                        manifest: builtin_manifest.clone(),
                        rule_source: RuleSource::BuiltIn,
                        rule_channel: RuleChannel::Bundled,
                        validation: RulesValidation::Valid,
                        last_refresh_at: None,
                        status: write_loaded_snapshot_status(
                            &path,
                            &snapshot,
                            "Rules cache was read, but its loaded timestamp could not be recorded.",
                        ),
                    };
                }

                match remote_snapshot_manifest(&snapshot, verifier) {
                    Ok(manifest) => {
                        snapshot.loaded_at = loaded_at;
                        let status = write_loaded_snapshot_status(
                            &path,
                            &snapshot,
                            "Remote rules cache was read, but its loaded timestamp could not be recorded.",
                        );
                        LoadedRulesCache {
                            manifest,
                            rule_source: RuleSource::Remote,
                            rule_channel: RuleChannel::Remote,
                            validation: RulesValidation::Valid,
                            last_refresh_at: Some(snapshot.updated_at.clone()),
                            status,
                        }
                    }
                    Err(warning) => builtin_with_status(
                        builtin_manifest,
                        RulesCacheStatus::invalid(loaded_at, warning),
                    ),
                }
            }
            Err(_) => builtin_with_status(
                builtin_manifest,
                RulesCacheStatus::invalid(
                    loaded_at,
                    "Remote rules cache was invalid; using the built-in manifest.",
                ),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => builtin_with_status(
            builtin_manifest,
            write_fresh_snapshot(
                &path,
                builtin_manifest,
                loaded_at,
                RulesCacheState::Recorded,
                None,
            ),
        ),
        Err(_) => builtin_with_status(
            builtin_manifest,
            RulesCacheStatus::unavailable_with_warning(
                loaded_at,
                "Remote rules cache could not be read; using the built-in manifest.",
            ),
        ),
    }
}

pub fn load_or_create_rules_cache(config_dir: &Path, manifest: &Manifest) -> RulesCacheStatus {
    let path = rules_cache_path(config_dir);
    let loaded_at = Utc::now().to_rfc3339();

    match fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<RulesCacheSnapshot>(&data) {
            Ok(mut snapshot) if snapshot_matches_manifest(&snapshot, manifest) => {
                snapshot.loaded_at = loaded_at;
                write_loaded_snapshot_status(
                    &path,
                    &snapshot,
                    "Rules cache was read, but its loaded timestamp could not be recorded.",
                )
            }
            Ok(_) | Err(_) => RulesCacheStatus::invalid(
                loaded_at,
                "Rules cache is invalid; using the built-in manifest.",
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_fresh_snapshot(&path, manifest, loaded_at, RulesCacheState::Recorded, None)
        }
        Err(_) => RulesCacheStatus::unavailable_with_warning(
            loaded_at,
            "Rules cache could not be read; using the built-in manifest.",
        ),
    }
}

pub fn write_remote_rules_cache(
    config_dir: &Path,
    manifest: &Manifest,
    signature: RulesSignatureMetadata,
) -> Result<RulesCacheStatus, std::io::Error> {
    let now = Utc::now().to_rfc3339();
    let snapshot = remote_snapshot(manifest, signature, now);
    let path = rules_cache_path(config_dir);
    write_snapshot(&path, &snapshot)?;
    Ok(RulesCacheStatus::from_snapshot(
        &snapshot,
        RulesCacheState::Recorded,
    ))
}

fn write_fresh_snapshot(
    path: &Path,
    manifest: &Manifest,
    loaded_at: String,
    state: RulesCacheState,
    warning: Option<&str>,
) -> RulesCacheStatus {
    let snapshot = builtin_snapshot(manifest, loaded_at);
    match write_snapshot(path, &snapshot) {
        Ok(()) => {
            let status = RulesCacheStatus::from_snapshot(&snapshot, state);
            if let Some(warning) = warning {
                status.with_warning(warning)
            } else {
                status
            }
        }
        Err(_) => RulesCacheStatus {
            recorded: false,
            state: RulesCacheState::Unavailable,
            updated_at: None,
            loaded_at: Some(snapshot.loaded_at),
            warning: Some("Rules cache could not be written locally.".to_string()),
        },
    }
}

fn builtin_snapshot(manifest: &Manifest, now: String) -> RulesCacheSnapshot {
    RulesCacheSnapshot {
        rule_source: RuleSource::BuiltIn,
        rule_channel: RuleChannel::Bundled,
        schema_version: manifest.schema_version,
        generated_at: manifest.generated_at.clone(),
        validation: RulesValidation::Valid,
        updated_at: now.clone(),
        loaded_at: now,
        manifest: None,
        signature: None,
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
        updated_at: now.clone(),
        loaded_at: now,
        manifest: Some(manifest.clone()),
        signature: Some(signature),
    }
}

fn snapshot_matches_manifest(snapshot: &RulesCacheSnapshot, manifest: &Manifest) -> bool {
    snapshot.rule_source == RuleSource::BuiltIn
        && snapshot.rule_channel == RuleChannel::Bundled
        && snapshot.schema_version == manifest.schema_version
        && snapshot.generated_at == manifest.generated_at
        && snapshot.validation == RulesValidation::Valid
        && !snapshot.updated_at.trim().is_empty()
        && !snapshot.loaded_at.trim().is_empty()
        && snapshot.manifest.is_none()
        && snapshot.signature.is_none()
}

fn remote_snapshot_manifest(
    snapshot: &RulesCacheSnapshot,
    verifier: &RemoteRulesVerifier,
) -> Result<Manifest, String> {
    if snapshot.rule_source != RuleSource::Remote
        || snapshot.rule_channel != RuleChannel::Remote
        || snapshot.validation != RulesValidation::Valid
        || snapshot.updated_at.trim().is_empty()
        || snapshot.loaded_at.trim().is_empty()
    {
        return Err("Remote rules cache was invalid; using the built-in manifest.".to_string());
    }

    let Some(manifest) = snapshot.manifest.clone() else {
        return Err(
            "Remote rules cache was missing its manifest; using the built-in manifest.".to_string(),
        );
    };
    let Some(signature) = snapshot.signature.as_ref() else {
        return Err(
            "Remote rules cache was missing its signature; using the built-in manifest."
                .to_string(),
        );
    };
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
    }
}

fn write_loaded_snapshot_status(
    path: &Path,
    snapshot: &RulesCacheSnapshot,
    warning: &'static str,
) -> RulesCacheStatus {
    match write_snapshot(path, snapshot) {
        Ok(()) => RulesCacheStatus::from_snapshot(snapshot, RulesCacheState::Recorded),
        Err(_) => RulesCacheStatus::from_snapshot(snapshot, RulesCacheState::Recorded)
            .with_warning(warning),
    }
}

fn write_snapshot(path: &Path, snapshot: &RulesCacheSnapshot) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(snapshot).expect("rules cache snapshot serializes");
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file(&temp_path, path)
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

fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }

    if destination.exists() {
        let _ = fs::remove_file(destination);
    }

    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(source);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RulesCacheSnapshot, RulesCacheState, load_active_rules_cache, load_or_create_rules_cache,
        rules_cache_path, write_remote_rules_cache,
    };
    use crate::resolve::builtin_manifest;
    use crate::signature::{RemoteRulesVerifier, RulesSignatureMetadata};
    use crate::status::{RuleChannel, RuleSource, RulesValidation};
    use ed25519_dalek::{Signer, SigningKey};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn missing_cache_is_created_from_builtin_manifest() {
        let root = test_root("missing");
        let manifest = builtin_manifest().expect("builtin manifest");

        let status = load_or_create_rules_cache(&root, &manifest);

        assert!(status.recorded);
        assert_eq!(status.state, RulesCacheState::Recorded);
        assert!(status.updated_at.is_some());
        assert!(status.loaded_at.is_some());
        assert!(status.warning.is_none());

        let snapshot = read_snapshot(&root);
        assert_eq!(snapshot.rule_source, RuleSource::BuiltIn);
        assert_eq!(snapshot.rule_channel, RuleChannel::Bundled);
        assert_eq!(snapshot.schema_version, manifest.schema_version);
        assert_eq!(snapshot.generated_at, manifest.generated_at);
        assert_eq!(snapshot.validation, RulesValidation::Valid);
        assert_eq!(snapshot.updated_at, status.updated_at.unwrap());
        assert_eq!(snapshot.loaded_at, status.loaded_at.unwrap());
        assert_eq!(snapshot.manifest, None);
        assert_eq!(snapshot.signature, None);
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(rules_cache_path(&root)).expect("read cache"))
                .expect("cache json");
        assert!(raw.get("manifest").is_some_and(serde_json::Value::is_null));
        assert!(raw.get("signature").is_some_and(serde_json::Value::is_null));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_cache_is_reported_without_repairing_file() {
        let root = test_root("invalid");
        let manifest = builtin_manifest().expect("builtin manifest");
        let path = rules_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(&path, "{not json").expect("write invalid cache");

        let status = load_or_create_rules_cache(&root, &manifest);

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
        let remote_status =
            write_remote_rules_cache(&root, &remote, signature).expect("write remote cache");

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
                loaded_at: "2026-05-30T10:00:00Z".to_string(),
                manifest: Some(remote),
                signature: Some(signature),
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
                loaded_at: "2026-05-30T10:00:00Z".to_string(),
                manifest: Some(remote),
                signature: None,
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
                .is_some_and(|warning| warning.contains("missing its signature"))
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
        assert!(loaded.status.recorded);
        assert!(
            loaded
                .status
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("public key is not configured"))
        );

        let _ = fs::remove_dir_all(root);
    }

    fn read_snapshot(root: &std::path::Path) -> RulesCacheSnapshot {
        let data = fs::read_to_string(rules_cache_path(root)).expect("read rules cache");
        serde_json::from_str(&data).expect("parse rules cache")
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "croopor-performance-rules-cache-{name}-{}-{nonce}",
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
