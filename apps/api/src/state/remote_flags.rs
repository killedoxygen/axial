use crate::observability::telemetry::{TelemetryHub, configured_posthog_environment};
use chrono::{DateTime, Utc};
use croopor_config::{FEATURE_FLAGS, FeatureFlagDef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use thiserror::Error;

const REMOTE_FLAGS_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const REMOTE_FLAGS_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_FLAGS_MAX_BYTES: usize = 1024 * 1024;
const REMOTE_FLAGS_USER_AGENT: &str =
    concat!("croopor/", env!("CARGO_PKG_VERSION"), " remote-flags");
const REMOTE_FLAGS_CACHE_FILE: &str = "remote-cache.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFlag {
    pub enabled: bool,
    pub source: ResolvedFlagSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFlagSource {
    Default,
    Override,
    Remote,
}

pub fn resolve_flag(
    flag: &FeatureFlagDef,
    overrides: &BTreeMap<String, bool>,
    remote_active: bool,
    remote_values: &BTreeMap<String, bool>,
) -> ResolvedFlag {
    if let Some(enabled) = overrides.get(flag.key).copied() {
        return ResolvedFlag {
            enabled,
            source: ResolvedFlagSource::Override,
        };
    }

    if remote_active
        && !flag.dev_only
        && let Some(enabled) = remote_values.get(flag.key).copied()
    {
        return ResolvedFlag {
            enabled,
            source: ResolvedFlagSource::Remote,
        };
    }

    ResolvedFlag {
        enabled: flag.default_enabled,
        source: ResolvedFlagSource::Default,
    }
}

pub struct RemoteFlagStore {
    cache_path: PathBuf,
    values: RwLock<BTreeMap<String, bool>>,
    fetched_at: RwLock<Option<String>>,
}

impl RemoteFlagStore {
    pub fn load_from_config_dir(config_dir: &Path) -> Self {
        let cache_path = remote_flags_cache_path(config_dir);
        let snapshot =
            load_remote_flags_cache_with_registry(&cache_path, Utc::now(), FEATURE_FLAGS);
        let (values, fetched_at) = snapshot
            .map(|snapshot| (snapshot.values, Some(snapshot.fetched_at)))
            .unwrap_or_default();

        Self {
            cache_path,
            values: RwLock::new(values),
            fetched_at: RwLock::new(fetched_at),
        }
    }

    pub fn values_snapshot(&self) -> BTreeMap<String, bool> {
        self.values
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn fetched_at(&self) -> Option<String> {
        self.fetched_at
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn refresh_once(
        &self,
        telemetry: &TelemetryHub,
    ) -> Result<RemoteFlagRefreshOutcome, RemoteFlagRefreshError> {
        let Some(key) = telemetry.configured_posthog_key() else {
            return Ok(RemoteFlagRefreshOutcome::Skipped);
        };
        let Some(distinct_id) = telemetry.current_telemetry_install_id() else {
            return Ok(RemoteFlagRefreshOutcome::Skipped);
        };
        let request = RemoteFlagFetchRequest {
            host: telemetry.configured_posthog_host(),
            key,
            distinct_id,
        };
        let values = fetch_remote_flags(&request).await?;
        let flag_count = values.len();
        let snapshot = RemoteFlagCacheSnapshot {
            fetched_at: Utc::now().to_rfc3339(),
            values,
        };

        write_remote_flags_cache(&self.cache_path, &snapshot)?;
        self.replace_snapshot(snapshot);

        Ok(RemoteFlagRefreshOutcome::Refreshed { flag_count })
    }

    fn replace_snapshot(&self, snapshot: RemoteFlagCacheSnapshot) {
        *self
            .values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot.values;
        *self
            .fetched_at
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot.fetched_at);
    }

    #[cfg(test)]
    pub(crate) fn replace_values_for_test(
        &self,
        values: BTreeMap<String, bool>,
        fetched_at: Option<String>,
    ) {
        *self
            .values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = values;
        *self
            .fetched_at
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fetched_at;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFlagRefreshOutcome {
    Refreshed { flag_count: usize },
    Skipped,
}

#[derive(Debug, Error)]
pub enum RemoteFlagRefreshError {
    #[error("request failed")]
    Request(#[from] reqwest::Error),
    #[error("http status {0}")]
    HttpStatus(u16),
    #[error("response too large")]
    ResponseTooLarge,
    #[error("response parse failed")]
    Parse(#[from] serde_json::Error),
    #[error("cache write failed")]
    Cache(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
struct RemoteFlagFetchRequest {
    host: String,
    key: String,
    distinct_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteFlagCacheSnapshot {
    fetched_at: String,
    values: BTreeMap<String, bool>,
}

#[derive(Debug, Deserialize)]
struct RemoteFlagsEnvelope {
    #[serde(default)]
    flags: BTreeMap<String, RemoteFlagEvaluation>,
    #[serde(default, rename = "errorsWhileComputingFlags")]
    _errors_while_computing_flags: bool,
}

#[derive(Debug, Deserialize)]
struct RemoteFlagEvaluation {
    enabled: Option<bool>,
}

pub fn remote_flags_cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("flags").join(REMOTE_FLAGS_CACHE_FILE)
}

async fn fetch_remote_flags(
    request: &RemoteFlagFetchRequest,
) -> Result<BTreeMap<String, bool>, RemoteFlagRefreshError> {
    let url = format!("{}/flags?v=2", request.host.trim_end_matches('/'));
    let response = remote_flags_client()
        .post(url)
        .json(&serde_json::json!({
            "api_key": request.key.as_str(),
            "distinct_id": request.distinct_id.as_str(),
            "properties": {
                "environment": configured_posthog_environment(),
            },
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(RemoteFlagRefreshError::HttpStatus(
            response.status().as_u16(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > REMOTE_FLAGS_MAX_BYTES as u64)
    {
        return Err(RemoteFlagRefreshError::ResponseTooLarge);
    }

    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > REMOTE_FLAGS_MAX_BYTES {
            return Err(RemoteFlagRefreshError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    let values = parse_remote_flag_response_values(&body)?;
    Ok(filter_registered_remote_values_with_registry(
        values,
        FEATURE_FLAGS,
    ))
}

fn parse_remote_flag_response_values(
    body: &[u8],
) -> Result<BTreeMap<String, bool>, serde_json::Error> {
    let envelope = serde_json::from_slice::<RemoteFlagsEnvelope>(body)?;
    Ok(envelope
        .flags
        .into_iter()
        .filter_map(|(key, evaluation)| evaluation.enabled.map(|enabled| (key, enabled)))
        .collect())
}

fn load_remote_flags_cache_with_registry(
    path: &Path,
    now: DateTime<Utc>,
    registry: &[FeatureFlagDef],
) -> Option<RemoteFlagCacheSnapshot> {
    let data = fs::read_to_string(path).ok()?;
    let mut snapshot = serde_json::from_str::<RemoteFlagCacheSnapshot>(&data).ok()?;
    let fetched_at = DateTime::parse_from_rfc3339(&snapshot.fetched_at)
        .ok()?
        .with_timezone(&Utc);
    let age = now.signed_duration_since(fetched_at).to_std().ok()?;
    if age > REMOTE_FLAGS_CACHE_TTL {
        return None;
    }

    snapshot.values = filter_registered_remote_values_with_registry(snapshot.values, registry);
    Some(snapshot)
}

fn write_remote_flags_cache(
    path: &Path,
    snapshot: &RemoteFlagCacheSnapshot,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(snapshot)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, data)?;
    replace_file(&temp_path, path)
}

fn filter_registered_remote_values_with_registry(
    values: BTreeMap<String, bool>,
    registry: &[FeatureFlagDef],
) -> BTreeMap<String, bool> {
    values
        .into_iter()
        .filter(|(key, _)| {
            registry
                .iter()
                .any(|flag| flag.key == key.as_str() && !flag.dev_only)
        })
        .collect()
}

fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    if fs::rename(source, destination).is_ok() {
        return Ok(());
    }

    if destination.exists() && !destination.is_dir() {
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

fn remote_flags_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(REMOTE_FLAGS_USER_AGENT)
                .timeout(REMOTE_FLAGS_HTTP_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::StatusCode, http::Uri, routing::post};
    use croopor_config::{AppConfig, FlagStage};
    use serde_json::Value;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;

    const TEST_KEY: &str = "remote.test";
    const DEV_KEY: &str = "remote.dev-only";
    const POSTHOG_KEY: &str = "phc_test";
    const INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    static TEST_REGISTRY: &[FeatureFlagDef] = &[
        FeatureFlagDef {
            key: TEST_KEY,
            title: "Remote test",
            description: "Remote test flag",
            stage: FlagStage::Beta,
            dev_only: false,
            default_enabled: true,
        },
        FeatureFlagDef {
            key: DEV_KEY,
            title: "Remote dev test",
            description: "Remote dev flag",
            stage: FlagStage::Experimental,
            dev_only: true,
            default_enabled: false,
        },
    ];

    #[test]
    fn resolver_prefers_override_then_remote_then_default_and_reports_source() {
        let flag = test_flag(false);
        let remote_values = BTreeMap::from([(TEST_KEY.to_string(), true)]);
        let overrides = BTreeMap::new();

        let remote = resolve_flag(&flag, &overrides, true, &remote_values);
        assert!(remote.enabled);
        assert_eq!(remote.source, ResolvedFlagSource::Remote);

        let default = resolve_flag(&flag, &overrides, false, &remote_values);
        assert!(!default.enabled);
        assert_eq!(default.source, ResolvedFlagSource::Default);

        let overrides = BTreeMap::from([(TEST_KEY.to_string(), false)]);
        let override_resolution = resolve_flag(&flag, &overrides, true, &remote_values);
        assert!(!override_resolution.enabled);
        assert_eq!(override_resolution.source, ResolvedFlagSource::Override);
    }

    #[test]
    fn resolver_distinguishes_remote_false_from_absent() {
        let flag = test_flag(true);
        let remote_values = BTreeMap::from([(TEST_KEY.to_string(), false)]);

        let resolution = resolve_flag(&flag, &BTreeMap::new(), true, &remote_values);

        assert!(!resolution.enabled);
        assert_eq!(resolution.source, ResolvedFlagSource::Remote);
    }

    #[test]
    fn resolver_ignores_remote_values_for_dev_only_flags() {
        let flag = FeatureFlagDef {
            key: DEV_KEY,
            title: "Remote dev test",
            description: "Remote dev flag",
            stage: FlagStage::Experimental,
            dev_only: true,
            default_enabled: false,
        };
        let remote_values = BTreeMap::from([(DEV_KEY.to_string(), true)]);

        let resolution = resolve_flag(&flag, &BTreeMap::new(), true, &remote_values);

        assert!(!resolution.enabled);
        assert_eq!(resolution.source, ResolvedFlagSource::Default);
    }

    #[test]
    fn remote_values_for_unknown_and_dev_only_keys_are_ignored() {
        let values = BTreeMap::from([
            (TEST_KEY.to_string(), true),
            (DEV_KEY.to_string(), true),
            ("unknown.flag".to_string(), true),
        ]);

        let filtered = filter_registered_remote_values_with_registry(values, TEST_REGISTRY);

        assert_eq!(filtered, BTreeMap::from([(TEST_KEY.to_string(), true)]));
    }

    #[test]
    fn cache_load_respects_ttl_and_filters_values() {
        let root = test_root("cache-ttl");
        let path = remote_flags_cache_path(&root);
        let now = Utc::now();
        let snapshot = RemoteFlagCacheSnapshot {
            fetched_at: (now - chrono::Duration::hours(1)).to_rfc3339(),
            values: BTreeMap::from([
                (TEST_KEY.to_string(), false),
                (DEV_KEY.to_string(), true),
                ("unknown.flag".to_string(), true),
            ]),
        };
        write_remote_flags_cache(&path, &snapshot).expect("write fresh cache");

        let loaded =
            load_remote_flags_cache_with_registry(&path, now, TEST_REGISTRY).expect("fresh cache");

        assert_eq!(
            loaded.values,
            BTreeMap::from([(TEST_KEY.to_string(), false)])
        );
        assert_eq!(loaded.fetched_at, snapshot.fetched_at);

        let stale = RemoteFlagCacheSnapshot {
            fetched_at: (now - chrono::Duration::hours(25)).to_rfc3339(),
            values: BTreeMap::from([(TEST_KEY.to_string(), true)]),
        };
        write_remote_flags_cache(&path, &stale).expect("write stale cache");

        assert!(load_remote_flags_cache_with_registry(&path, now, TEST_REGISTRY).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_rejects_unknown_fields_and_unparseable_timestamps() {
        let root = test_root("cache-junk");
        let path = remote_flags_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "fetched_at": Utc::now().to_rfc3339(),
                "values": { "remote.test": true },
                "junk": true
            }))
            .expect("serialize junk cache"),
        )
        .expect("write junk cache");
        assert!(load_remote_flags_cache_with_registry(&path, Utc::now(), TEST_REGISTRY).is_none());

        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "fetched_at": "not a timestamp",
                "values": { "remote.test": true }
            }))
            .expect("serialize bad timestamp cache"),
        )
        .expect("write bad timestamp cache");
        assert!(load_remote_flags_cache_with_registry(&path, Utc::now(), TEST_REGISTRY).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_write_is_atomic_and_contains_only_timestamp_and_values() {
        let root = test_root("cache-write");
        let path = remote_flags_cache_path(&root);
        let old = RemoteFlagCacheSnapshot {
            fetched_at: "2026-01-01T00:00:00Z".to_string(),
            values: BTreeMap::from([(TEST_KEY.to_string(), true)]),
        };
        write_remote_flags_cache(&path, &old).expect("write old cache");
        let next = RemoteFlagCacheSnapshot {
            fetched_at: "2026-01-02T00:00:00Z".to_string(),
            values: BTreeMap::from([(TEST_KEY.to_string(), false)]),
        };

        write_remote_flags_cache(&path, &next).expect("write next cache");

        let raw = fs::read_to_string(&path).expect("read cache");
        let value = serde_json::from_str::<Value>(&raw).expect("cache json");
        let object = value.as_object().expect("cache object");
        assert_eq!(object.len(), 2);
        assert!(object.contains_key("fetched_at"));
        assert!(object.contains_key("values"));
        assert_eq!(value["values"][TEST_KEY], false);
        assert!(!path.with_extension("json.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn store_tracks_values_and_fetch_timestamp() {
        let root = test_root("store-snapshot");
        let paths = test_paths(&root);
        let store = RemoteFlagStore::load_from_config_dir(&paths.config_dir);
        let fetched_at = "2026-01-02T00:00:00Z".to_string();

        store.replace_values_for_test(
            BTreeMap::from([(TEST_KEY.to_string(), false)]),
            Some(fetched_at.clone()),
        );

        assert_eq!(
            store.values_snapshot(),
            BTreeMap::from([(TEST_KEY.to_string(), false)])
        );
        assert_eq!(store.fetched_at(), Some(fetched_at));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn response_parser_tolerates_v2_extra_fields_and_preserves_enabled_false() {
        let body = serde_json::to_vec(&serde_json::json!({
            "flags": {
                "remote.test": {
                    "key": TEST_KEY,
                    "enabled": false,
                    "variant": "ignored",
                    "reason": { "code": "condition_match" }
                },
                "missing.enabled": {
                    "key": "missing.enabled"
                }
            },
            "errorsWhileComputingFlags": true,
            "extra": "ignored"
        }))
        .expect("serialize response");

        let values = parse_remote_flag_response_values(&body).expect("parse response");

        assert_eq!(values.get(TEST_KEY), Some(&false));
        assert!(!values.contains_key("missing.enabled"));
    }

    #[tokio::test]
    async fn fetch_posts_posthog_flags_v2_body_shape() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping socket remote flag fetch test: bind denied");
                return;
            }
            Err(error) => panic!("bind remote flag test server: {error}"),
        };
        let addr = listener.local_addr().expect("test listener addr");
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Option<String>, Value)>();
        let app = Router::new()
            .route("/flags", post(capture_flags))
            .with_state(tx);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let request = RemoteFlagFetchRequest {
            host: format!("http://{addr}"),
            key: POSTHOG_KEY.to_string(),
            distinct_id: INSTALL_ID.to_string(),
        };

        let values = fetch_remote_flags(&request).await.expect("fetch flags");

        let (path, query, body) = rx.recv().await.expect("captured flags request");
        server.abort();

        assert_eq!(path, "/flags");
        assert_eq!(query.as_deref(), Some("v=2"));
        assert_eq!(body["api_key"], POSTHOG_KEY);
        assert!(body.get("token").is_none());
        assert_eq!(body["distinct_id"], INSTALL_ID);
        assert_eq!(
            body["properties"]["environment"],
            configured_posthog_environment()
        );
        assert!(values.is_empty());
    }

    #[tokio::test]
    async fn refresh_skips_when_install_id_is_empty_without_generating_one() {
        let root = test_root("empty-install-id");
        let paths = test_paths(&root);
        let config = Arc::new(
            croopor_config::ConfigStore::load_from(paths.clone()).expect("load config store"),
        );
        config
            .replace_in_memory(AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: String::new(),
                ..AppConfig::default()
            })
            .expect("seed config");
        let telemetry = TelemetryHub::new(
            config.clone(),
            Some(POSTHOG_KEY.to_string()),
            "http://127.0.0.1:9".to_string(),
        );
        let store = RemoteFlagStore::load_from_config_dir(&paths.config_dir);

        assert_eq!(
            store.refresh_once(&telemetry).await.expect("skip refresh"),
            RemoteFlagRefreshOutcome::Skipped
        );
        assert!(config.current().telemetry_install_id.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    async fn capture_flags(
        State(tx): State<mpsc::UnboundedSender<(String, Option<String>, Value)>>,
        uri: Uri,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let _ = tx.send((
            uri.path().to_string(),
            uri.query().map(str::to_string),
            body,
        ));
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "flags": {
                    "dev.state-inspector": {
                        "key": "dev.state-inspector",
                        "enabled": true
                    }
                },
                "errorsWhileComputingFlags": true
            })),
        )
    }

    fn test_flag(default_enabled: bool) -> FeatureFlagDef {
        FeatureFlagDef {
            key: TEST_KEY,
            title: "Remote test",
            description: "Remote test flag",
            stage: FlagStage::Beta,
            dev_only: false,
            default_enabled,
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "croopor-api-remote-flags-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &Path) -> croopor_config::AppPaths {
        let config_dir = root.join("config");
        croopor_config::AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
