use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::paths::version_manifest_cache_path;

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const CACHE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifest {
    pub latest: LatestVersions,
    pub versions: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestVersions {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub time: String,
    #[serde(rename = "releaseTime", default)]
    pub release_time: String,
    #[serde(default)]
    pub sha1: String,
    #[serde(rename = "complianceLevel", default)]
    pub compliance_level: i32,
}

#[derive(Debug)]
struct ManifestCache {
    value: Option<VersionManifest>,
    fetched_at: Option<Instant>,
}

static MANIFEST_CACHE: OnceLock<Mutex<ManifestCache>> = OnceLock::new();

pub async fn fetch_version_manifest() -> Result<VersionManifest, String> {
    if let Some(value) = fresh_cached_manifest() {
        return Ok(value);
    }

    let cached_stale = stale_cached_manifest();
    let manifest = match fetch_manifest_live().await {
        Ok(manifest) => manifest,
        Err(error) => {
            if let Some(stale) = cached_stale {
                return Ok(stale);
            }
            return Err(error);
        }
    };

    update_manifest_cache(manifest.clone());
    Ok(manifest)
}

pub async fn fetch_version_manifest_cached(library_dir: &Path) -> Result<VersionManifest, String> {
    let cache_path = version_manifest_cache_path(library_dir);

    if let Some(value) = fresh_cached_manifest() {
        let _ = write_persistent_manifest_cache_value(&cache_path, &value);
        return Ok(value);
    }

    let cached_stale = stale_cached_manifest();
    let (manifest, live_body) = resolve_manifest_from_live_or_cache(
        &cache_path,
        fetch_manifest_live_body().await,
        cached_stale,
    )?;

    if let Some(live_body) = live_body {
        let _ = write_persistent_manifest_cache(&cache_path, &live_body);
    }

    update_manifest_cache(manifest.clone());
    Ok(manifest)
}

fn fresh_cached_manifest() -> Option<VersionManifest> {
    let cache = MANIFEST_CACHE.get_or_init(|| {
        Mutex::new(ManifestCache {
            value: None,
            fetched_at: None,
        })
    });

    if let Ok(cache) = cache.lock()
        && let (Some(value), Some(fetched_at)) = (&cache.value, cache.fetched_at)
        && fetched_at.elapsed() < CACHE_TTL
    {
        return Some(value.clone());
    }
    None
}

fn stale_cached_manifest() -> Option<VersionManifest> {
    let cache = MANIFEST_CACHE.get_or_init(|| {
        Mutex::new(ManifestCache {
            value: None,
            fetched_at: None,
        })
    });
    cache.lock().ok().and_then(|cache| cache.value.clone())
}

fn update_manifest_cache(manifest: VersionManifest) {
    let cache = MANIFEST_CACHE.get_or_init(|| {
        Mutex::new(ManifestCache {
            value: None,
            fetched_at: None,
        })
    });
    if let Ok(mut cache) = cache.lock() {
        cache.value = Some(manifest);
        cache.fetched_at = Some(Instant::now());
    }
}

async fn fetch_manifest_live() -> Result<VersionManifest, String> {
    let body = fetch_manifest_live_body().await?;
    parse_manifest_body(&body)
}

async fn fetch_manifest_live_body() -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(|| {
        let response = manifest_agent()
            .get(MANIFEST_URL)
            .call()
            .map_err(|error| format!("fetching version manifest: {error}"))?;

        let mut reader = response.into_reader();
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .map_err(|error| format!("reading version manifest: {error}"))?;
        Ok(body)
    })
    .await
    .map_err(|error| format!("fetching version manifest: {error}"))?
}

fn resolve_manifest_from_live_or_cache(
    cache_path: &Path,
    live_result: Result<Vec<u8>, String>,
    stale_manifest: Option<VersionManifest>,
) -> Result<(VersionManifest, Option<Vec<u8>>), String> {
    match live_result {
        Ok(body) => match parse_manifest_body(&body) {
            Ok(manifest) => Ok((manifest, Some(body))),
            Err(error) => read_persistent_manifest_cache(cache_path)
                .map(|manifest| (manifest, None))
                .or_else(|_| {
                    stale_manifest
                        .clone()
                        .map(|manifest| (manifest, None))
                        .ok_or(error)
                }),
        },
        Err(error) => read_persistent_manifest_cache(cache_path)
            .map(|manifest| (manifest, None))
            .or_else(|_| stale_manifest.map(|manifest| (manifest, None)).ok_or(error)),
    }
}

fn read_persistent_manifest_cache(path: &Path) -> Result<VersionManifest, String> {
    let data =
        fs::read(path).map_err(|error| format!("reading cached version manifest: {error}"))?;
    parse_manifest_body(&data)
}

fn write_persistent_manifest_cache(path: &Path, data: &[u8]) -> Result<(), String> {
    parse_manifest_body(data)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("creating version manifest cache directory: {error}"))?;
    }

    let tmp_path = manifest_cache_tmp_path(path);
    let result = (|| -> Result<(), String> {
        fs::write(&tmp_path, data)
            .map_err(|error| format!("writing version manifest cache: {error}"))?;
        promote_manifest_cache_tmp_file(&tmp_path, path)
            .map_err(|error| format!("promoting version manifest cache: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn write_persistent_manifest_cache_value(
    path: &Path,
    manifest: &VersionManifest,
) -> Result<(), String> {
    let data = serde_json::to_vec(manifest)
        .map_err(|error| format!("serializing version manifest cache: {error}"))?;
    write_persistent_manifest_cache(path, &data)
}

fn promote_manifest_cache_tmp_file(tmp_path: &Path, path: &Path) -> std::io::Result<()> {
    let first_error = match fs::rename(tmp_path, path) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match fs::symlink_metadata(tmp_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(first_error),
        Err(error) => return Err(error),
    }

    if let Ok(metadata) = fs::symlink_metadata(path) {
        let file_type = metadata.file_type();
        if file_type.is_file() || file_type.is_symlink() {
            fs::remove_file(path)?;
        }
    }

    let result = fs::rename(tmp_path, path);
    if result.is_err() {
        let _ = fs::remove_file(tmp_path);
    }
    result
}

fn manifest_cache_tmp_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let extension = format!("tmp-{}-{nanos:x}", std::process::id());
    path.with_extension(extension)
}

fn parse_manifest_body(data: &[u8]) -> Result<VersionManifest, String> {
    serde_json::from_slice::<VersionManifest>(data)
        .map_err(|error| format!("parsing version manifest: {error}"))
}

fn manifest_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(15))
            .timeout_write(Duration::from_secs(15))
            .user_agent("croopor/0.3")
            .build()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_persistent_manifest_cache() {
        let root = temp_dir("manifest-cache-round-trip");
        let cache_path = root.join("cache").join("version_manifest_v2.json");
        let body = sample_manifest_body("1.21.5");

        write_persistent_manifest_cache(&cache_path, body.as_bytes()).expect("write cache");
        let manifest = read_persistent_manifest_cache(&cache_path).expect("read cache");

        assert_eq!(manifest.latest.release, "1.21.5");
        assert_eq!(manifest.versions[0].id, "1.21.5");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fallback_returns_cached_manifest_when_live_provider_fails() {
        let root = temp_dir("manifest-cache-fallback");
        let cache_path = root.join("version_manifest_v2.json");
        write_persistent_manifest_cache(&cache_path, sample_manifest_body("1.21.4").as_bytes())
            .expect("write cache");

        let (manifest, live_body) = resolve_manifest_from_live_or_cache(
            &cache_path,
            Err("fetching version manifest: offline".to_string()),
            None,
        )
        .expect("cached manifest should satisfy provider failure");

        assert_eq!(manifest.latest.release, "1.21.4");
        assert!(live_body.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fresh_manifest_value_can_seed_a_requested_library_cache() {
        let root = temp_dir("manifest-cache-fresh-library");
        let cache_path = root.join("cache").join("version_manifest_v2.json");
        let manifest =
            parse_manifest_body(sample_manifest_body("1.21.6").as_bytes()).expect("parse manifest");

        write_persistent_manifest_cache_value(&cache_path, &manifest).expect("write cache value");

        let cached = read_persistent_manifest_cache(&cache_path).expect("read cache");
        assert_eq!(cached.latest.release, "1.21.6");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_cache_does_not_mask_original_provider_error() {
        let root = temp_dir("manifest-cache-corrupt");
        let cache_path = root.join("version_manifest_v2.json");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&cache_path, b"not json").expect("write corrupt cache");

        let error = resolve_manifest_from_live_or_cache(
            &cache_path,
            Err("fetching version manifest: offline".to_string()),
            None,
        )
        .expect_err("corrupt cache should not replace provider error");

        assert_eq!(error, "fetching version manifest: offline");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_parse_error_can_fall_back_to_cached_manifest() {
        let root = temp_dir("manifest-cache-live-parse");
        let cache_path = root.join("version_manifest_v2.json");
        write_persistent_manifest_cache(&cache_path, sample_manifest_body("1.21.3").as_bytes())
            .expect("write cache");

        let (manifest, live_body) =
            resolve_manifest_from_live_or_cache(&cache_path, Ok(b"not json".to_vec()), None)
                .expect("cached manifest should satisfy live parse failure");

        assert_eq!(manifest.latest.release, "1.21.3");
        assert!(live_body.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temp_promotion_replaces_existing_manifest_cache() {
        let root = temp_dir("manifest-cache-promote");
        fs::create_dir_all(&root).expect("create root");
        let tmp_path = root.join("version_manifest_v2.tmp");
        let cache_path = root.join("version_manifest_v2.json");
        fs::write(&cache_path, b"old").expect("write old cache");
        fs::write(&tmp_path, b"new").expect("write temp cache");

        promote_manifest_cache_tmp_file(&tmp_path, &cache_path).expect("promote temp cache");

        assert_eq!(fs::read(&cache_path).expect("read promoted cache"), b"new");
        assert!(!tmp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temp_promotion_preserves_destination_when_temp_is_missing() {
        let root = temp_dir("manifest-cache-missing-temp");
        fs::create_dir_all(&root).expect("create root");
        let tmp_path = root.join("missing.tmp");
        let cache_path = root.join("version_manifest_v2.json");
        fs::write(&cache_path, b"old").expect("write old cache");

        let error = promote_manifest_cache_tmp_file(&tmp_path, &cache_path)
            .expect_err("missing temp should fail");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(fs::read(&cache_path).expect("read old cache"), b"old");

        let _ = fs::remove_dir_all(root);
    }

    fn sample_manifest_body(release: &str) -> String {
        format!(
            r#"{{
  "latest": {{ "release": "{release}", "snapshot": "25w21a" }},
  "versions": [
    {{
      "id": "{release}",
      "type": "release",
      "url": "https://example.invalid/{release}.json",
      "time": "2026-01-01T00:00:00+00:00",
      "releaseTime": "2026-01-01T00:00:00+00:00",
      "sha1": "abc123",
      "complianceLevel": 1
    }}
  ]
}}"#
        )
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "croopor-manifest-cache-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }
}
