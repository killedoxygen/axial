use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::http;
use crate::managed::fs::{ManagedDir, ManagedFileGuard, ManagedLibraryOperation};

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const MANIFEST_CACHE_NAME: &str = "version_manifest_v2.json";
const CACHE_TTL: Duration = Duration::from_secs(600);
const MAX_MANIFEST_BYTES: u64 = 8 << 20;

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

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("version manifest acquisition was cancelled")]
    Cancelled,
    #[error("{0}")]
    Unavailable(String),
}

pub(crate) async fn fetch_fresh_install_version_manifest() -> Result<VersionManifest, String> {
    fetch_manifest_live().await
}

#[cfg(feature = "test-support")]
pub(crate) async fn fetch_fresh_install_version_manifest_at_test_endpoint(
    endpoints: &crate::download::InstallTestEndpoints,
) -> Result<VersionManifest, String> {
    let body = fetch_manifest_body(endpoints.version_manifest(), &http::RETRY_DELAYS).await?;
    parse_manifest_body(&body)
}

pub async fn fetch_version_manifest_cached(
    operation: &ManagedLibraryOperation,
) -> Result<VersionManifest, String> {
    fetch_version_manifest_cached_cancellable(operation, std::future::pending())
        .await
        .map_err(|error| error.to_string())
}

/// Only live acquisition is cancellable; cache publication remains owned through settlement.
pub async fn fetch_version_manifest_cached_cancellable(
    operation: &ManagedLibraryOperation,
    cancelled: impl Future<Output = ()>,
) -> Result<VersionManifest, FetchError> {
    fetch_version_manifest_cached_with(operation, cancelled, true, fetch_manifest_live_body()).await
}

#[cfg(feature = "test-support")]
pub async fn fetch_version_manifest_cached_from_loopback_for_test(
    operation: &ManagedLibraryOperation,
    url: &reqwest::Url,
    cancelled: impl Future<Output = ()>,
) -> Result<VersionManifest, FetchError> {
    crate::download::TransferOrigin::from_loopback_http_for_test_support(url).map_err(|_| {
        FetchError::Unavailable("manifest test source must use literal loopback HTTP".into())
    })?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(FetchError::Unavailable(
            "manifest test source cannot contain query or fragment".into(),
        ));
    }
    fetch_version_manifest_cached_with(
        operation,
        cancelled,
        false,
        fetch_manifest_body(url.as_str(), &[]),
    )
    .await
}

async fn fetch_version_manifest_cached_with(
    operation: &ManagedLibraryOperation,
    cancelled: impl Future<Output = ()>,
    shared_cache: bool,
    fetch_body: impl Future<Output = Result<Vec<u8>, String>>,
) -> Result<VersionManifest, FetchError> {
    if let Some(value) = fresh_persistent_manifest_cache(operation) {
        if shared_cache {
            update_manifest_cache(value.clone());
        }
        return Ok(value);
    }

    if let Some(value) = shared_cache.then(fresh_cached_manifest).flatten() {
        let _ = write_persistent_manifest_cache_value(operation, &value).await;
        return Ok(value);
    }

    let stale = read_persistent_manifest_cache(operation)
        .ok()
        .or_else(|| shared_cache.then(stale_cached_manifest).flatten());
    let live = tokio::select! {
        biased;
        result = fetch_body => result,
        _ = cancelled => return Err(FetchError::Cancelled),
    };
    let (manifest, live_body) = if let Some(stale) = stale {
        match live.and_then(|body| parse_manifest_body(&body).map(|manifest| (manifest, body))) {
            Ok((manifest, body)) => (manifest, Some(body)),
            Err(_) => return Ok(stale),
        }
    } else {
        resolve_manifest_from_live_or_cache(operation, live, None)
            .map_err(FetchError::Unavailable)?
    };

    if let Some(live_body) = live_body {
        let _ = write_persistent_manifest_cache(operation, &live_body).await;
    }

    if shared_cache {
        update_manifest_cache(manifest.clone());
    }
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

fn fresh_persistent_manifest_cache(operation: &ManagedLibraryOperation) -> Option<VersionManifest> {
    let cache = open_manifest_cache(operation).ok()?;
    let guard = cache.inspect_regular_file(MANIFEST_CACHE_NAME).ok()??;
    if !manifest_cache_timestamp_is_fresh(guard.modified_at_ns().ok()?, SystemTime::now()) {
        return None;
    }
    read_persistent_manifest_cache_from_guard(&cache, &guard).ok()
}

fn manifest_cache_timestamp_is_fresh(modified_at_ns: u64, now: SystemTime) -> bool {
    let Ok(now) = now.duration_since(UNIX_EPOCH) else {
        return false;
    };
    let now_ns = now.as_nanos();
    let modified_at_ns = u128::from(modified_at_ns);
    modified_at_ns <= now_ns && now_ns - modified_at_ns < CACHE_TTL.as_nanos()
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
    fetch_manifest_body(MANIFEST_URL, &http::RETRY_DELAYS).await
}

async fn fetch_manifest_body(url: &str, retry_delays: &[Duration]) -> Result<Vec<u8>, String> {
    let fetch = || async {
        http::fetch_once(
            http::client_for(url)?,
            url,
            MAX_MANIFEST_BYTES,
            http::Expected::default(),
        )
        .await
    };
    http::retry(retry_delays, fetch, http::FetchError::is_retryable)
        .await
        .map(|fetched| fetched.bytes)
        .map_err(|error| match error.status() {
            Some(status) => format!("fetching version manifest: HTTP {status}"),
            None => format!("fetching version manifest: {error}"),
        })
}

fn resolve_manifest_from_live_or_cache(
    operation: &ManagedLibraryOperation,
    live_result: Result<Vec<u8>, String>,
    stale_manifest: Option<VersionManifest>,
) -> Result<(VersionManifest, Option<Vec<u8>>), String> {
    match live_result {
        Ok(body) => match parse_manifest_body(&body) {
            Ok(manifest) => Ok((manifest, Some(body))),
            Err(error) => read_persistent_manifest_cache(operation)
                .map(|manifest| (manifest, None))
                .or_else(|_| {
                    stale_manifest
                        .clone()
                        .map(|manifest| (manifest, None))
                        .ok_or(error)
                }),
        },
        Err(error) => read_persistent_manifest_cache(operation)
            .map(|manifest| (manifest, None))
            .or_else(|_| stale_manifest.map(|manifest| (manifest, None)).ok_or(error)),
    }
}

fn open_manifest_cache(operation: &ManagedLibraryOperation) -> Result<ManagedDir, String> {
    operation
        .managed_directory()
        .and_then(|root| root.open_child("cache"))
        .map_err(|error| format!("opening cached version manifest: {error}"))
}

fn open_or_create_manifest_cache(
    operation: &ManagedLibraryOperation,
) -> Result<ManagedDir, String> {
    operation
        .managed_directory()
        .and_then(|root| root.open_or_create_child("cache"))
        .map_err(|error| format!("opening cached version manifest: {error}"))
}

fn read_persistent_manifest_cache(
    operation: &ManagedLibraryOperation,
) -> Result<VersionManifest, String> {
    let cache = open_manifest_cache(operation)?;
    read_persistent_manifest_cache_from(&cache)
}

/// Read the exact admitted cache bytes and their freshness independently of live I/O.
/// Invalid JSON remains observable to the caller rather than silently becoming an empty catalog.
pub fn read_cached_manifest_bytes(
    operation: &ManagedLibraryOperation,
) -> Result<(Vec<u8>, bool), String> {
    let cache = open_manifest_cache(operation)?;
    let guard = cache
        .inspect_regular_file(MANIFEST_CACHE_NAME)
        .map_err(|error| format!("reading cached version manifest: {error}"))?
        .ok_or_else(|| "reading cached version manifest: cache is missing".to_string())?;
    let fresh = manifest_cache_timestamp_is_fresh(
        guard.modified_at_ns().map_err(|error| error.to_string())?,
        SystemTime::now(),
    );
    let bytes = cache
        .read_guarded_file_bounded(MANIFEST_CACHE_NAME, &guard, MAX_MANIFEST_BYTES)
        .map_err(|error| format!("reading cached version manifest: {error}"))?;
    Ok((bytes, fresh))
}

/// Validate and publish a manifest cache through the admitted library capability.
pub async fn cache_version_manifest(
    operation: &ManagedLibraryOperation,
    bytes: &[u8],
) -> Result<(), String> {
    write_persistent_manifest_cache(operation, bytes).await?;
    update_manifest_cache(parse_manifest_body(bytes)?);
    Ok(())
}

fn read_persistent_manifest_cache_from(cache: &ManagedDir) -> Result<VersionManifest, String> {
    let guard = cache
        .inspect_regular_file(MANIFEST_CACHE_NAME)
        .map_err(|error| format!("reading cached version manifest: {error}"))?
        .ok_or_else(|| "reading cached version manifest: cache is missing".to_string())?;
    read_persistent_manifest_cache_from_guard(cache, &guard)
}

fn read_persistent_manifest_cache_from_guard(
    cache: &ManagedDir,
    guard: &ManagedFileGuard,
) -> Result<VersionManifest, String> {
    let data = cache
        .read_guarded_file_bounded(MANIFEST_CACHE_NAME, guard, MAX_MANIFEST_BYTES)
        .map_err(|error| format!("reading cached version manifest: {error}"))?;
    parse_manifest_body(&data)
}

async fn write_persistent_manifest_cache(
    operation: &ManagedLibraryOperation,
    data: &[u8],
) -> Result<(), String> {
    validate_manifest_cache_bytes(data)?;
    open_or_create_manifest_cache(operation)?
        .write_exact(MANIFEST_CACHE_NAME, data)
        .await
        .map_err(|error| format!("writing cached version manifest: {error}"))
}

fn validate_manifest_cache_bytes(data: &[u8]) -> Result<(), String> {
    if data.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("reading cached version manifest: response too large".to_string());
    }
    parse_manifest_body(data)?;
    Ok(())
}

async fn write_persistent_manifest_cache_value(
    operation: &ManagedLibraryOperation,
    manifest: &VersionManifest,
) -> Result<(), String> {
    let data = serde_json::to_vec(manifest)
        .map_err(|error| format!("serializing version manifest cache: {error}"))?;
    write_persistent_manifest_cache(operation, &data).await
}

#[cfg(feature = "test-support")]
pub fn persist_version_manifest_cache_fixture_for_test(
    operation: &ManagedLibraryOperation,
    data: &[u8],
) -> Result<(), String> {
    validate_manifest_cache_bytes(data)?;
    open_or_create_manifest_cache(operation)?
        .write_exact_fixture(MANIFEST_CACHE_NAME, data)
        .map_err(|error| format!("writing cached version manifest: {error}"))
}

fn parse_manifest_body(data: &[u8]) -> Result<VersionManifest, String> {
    serde_json::from_slice::<VersionManifest>(data)
        .map_err(|error| format!("parsing version manifest: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_server::{response, serve};
    use crate::managed::fs::ManagedLibraryRoot;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[tokio::test]
    async fn writes_and_reads_persistent_manifest_cache() {
        let root = temp_dir("manifest-cache-round-trip");
        let (_library, operation) = test_library(&root);
        let body = sample_manifest_body("1.21.5");

        write_persistent_manifest_cache(&operation, body.as_bytes())
            .await
            .expect("write cache");
        let manifest = read_persistent_manifest_cache(&operation).expect("read cache");

        assert_eq!(manifest.latest.release, "1.21.5");
        assert_eq!(manifest.versions[0].id, "1.21.5");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fallback_returns_cached_manifest_when_live_provider_fails() {
        let root = temp_dir("manifest-cache-fallback");
        let (_library, operation) = test_library(&root);
        write_persistent_manifest_cache(&operation, sample_manifest_body("1.21.4").as_bytes())
            .await
            .expect("write cache");

        let (manifest, live_body) = resolve_manifest_from_live_or_cache(
            &operation,
            Err("fetching version manifest: offline".to_string()),
            None,
        )
        .expect("cached manifest should satisfy provider failure");

        assert_eq!(manifest.latest.release, "1.21.4");
        assert!(live_body.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_manifest_value_can_seed_a_requested_library_cache() {
        let root = temp_dir("manifest-cache-fresh-library");
        let (_library, operation) = test_library(&root);
        let manifest =
            parse_manifest_body(sample_manifest_body("1.21.6").as_bytes()).expect("parse manifest");

        write_persistent_manifest_cache_value(&operation, &manifest)
            .await
            .expect("write cache value");

        let cached = read_persistent_manifest_cache(&operation).expect("read cache");
        assert_eq!(cached.latest.release, "1.21.6");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_persistent_manifest_is_preferred_for_requested_library() {
        let root = temp_dir("manifest-cache-fresh-local-first");
        let (_library, operation) = test_library(&root);
        write_persistent_manifest_cache(&operation, sample_manifest_body("1.21.7").as_bytes())
            .await
            .expect("write cache");

        let manifest = fresh_persistent_manifest_cache(&operation).expect("fresh local manifest");

        assert_eq!(manifest.latest.release, "1.21.7");
        assert_eq!(manifest.versions[0].id, "1.21.7");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persistent_manifest_timestamp_rejects_stale_and_future_files() {
        let now = UNIX_EPOCH + Duration::from_secs(20_000);
        let now_ns = now
            .duration_since(UNIX_EPOCH)
            .expect("test time")
            .as_nanos() as u64;

        assert!(manifest_cache_timestamp_is_fresh(
            now_ns - CACHE_TTL.as_nanos() as u64 + 1,
            now,
        ));
        assert!(!manifest_cache_timestamp_is_fresh(
            now_ns - CACHE_TTL.as_nanos() as u64,
            now,
        ));
        assert!(!manifest_cache_timestamp_is_fresh(now_ns + 1, now));
    }

    #[test]
    fn persistent_manifest_cache_rejects_oversized_bytes_before_parsing() {
        let oversized = vec![b' '; MAX_MANIFEST_BYTES as usize + 1];

        assert_eq!(
            validate_manifest_cache_bytes(&oversized).expect_err("oversized cache"),
            "reading cached version manifest: response too large"
        );
    }

    #[test]
    fn corrupt_capability_cache_does_not_mask_original_provider_error() {
        let root = temp_dir("manifest-cache-corrupt");
        let (_library, operation) = test_library(&root);
        open_or_create_manifest_cache(&operation)
            .expect("open cache")
            .write_exact_fixture(MANIFEST_CACHE_NAME, b"not json")
            .expect("write corrupt cache");

        let error = resolve_manifest_from_live_or_cache(
            &operation,
            Err("fetching version manifest: offline".to_string()),
            None,
        )
        .expect_err("corrupt cache should not replace provider error");

        assert_eq!(error, "fetching version manifest: offline");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn live_parse_error_can_fall_back_to_cached_manifest() {
        let root = temp_dir("manifest-cache-live-parse");
        let (_library, operation) = test_library(&root);
        write_persistent_manifest_cache(&operation, sample_manifest_body("1.21.3").as_bytes())
            .await
            .expect("write cache");

        let (manifest, live_body) =
            resolve_manifest_from_live_or_cache(&operation, Ok(b"not json".to_vec()), None)
                .expect("cached manifest should satisfy live parse failure");

        assert_eq!(manifest.latest.release, "1.21.3");
        assert!(live_body.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_persistent_cache_falls_back_when_synchronous_refresh_fails() {
        let root = temp_dir("manifest-cache-swr");
        let (_library, operation) = test_library(&root);
        write_persistent_manifest_cache(&operation, sample_manifest_body("1.21.8").as_bytes())
            .await
            .expect("write cache");
        fs::File::open(root.join("cache").join(MANIFEST_CACHE_NAME))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        let manifest = fetch_version_manifest_cached_with(
            &operation,
            std::future::pending(),
            false,
            fetch_manifest_body("http://127.0.0.1:9/version_manifest_v2.json", &[]),
        )
        .await
        .unwrap();

        assert_eq!(manifest.latest.release, "1.21.8");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_manifest_acquisition_preserves_absent_or_stale_cache() {
        for stale in [false, true] {
            let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
            let (_library, operation) = test_library(temporary.path());
            let path = temporary.path().join("cache").join(MANIFEST_CACHE_NAME);
            let body = sample_manifest_body("1.21.8");
            if stale {
                write_persistent_manifest_cache(&operation, body.as_bytes())
                    .await
                    .unwrap();
                fs::File::open(&path)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                    .unwrap();
                assert_eq!(
                    read_cached_manifest_bytes(&operation).unwrap(),
                    (body.as_bytes().to_vec(), false)
                );
            }
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                fetch_version_manifest_cached_with(
                    &operation,
                    std::future::ready(()),
                    false,
                    std::future::pending(),
                ),
            )
            .await
            .expect("manifest acquisition must settle on cancellation");
            assert!(
                matches!(result, Err(FetchError::Cancelled)),
                "cancellation cannot serve stale metadata"
            );
            if stale {
                assert_eq!(
                    read_cached_manifest_bytes(&operation).unwrap(),
                    (body.as_bytes().to_vec(), false)
                );
            } else {
                assert!(
                    fs::symlink_metadata(&path)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                );
            }
        }
    }

    #[tokio::test]
    async fn stale_persistent_cache_is_replaced_by_synchronous_refresh() {
        let root = temp_dir("manifest-cache-synchronous-refresh");
        let (_library, operation) = test_library(&root);
        write_persistent_manifest_cache(&operation, sample_manifest_body("1.21.8").as_bytes())
            .await
            .expect("write cache");
        fs::File::open(root.join("cache").join(MANIFEST_CACHE_NAME))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        let server = serve(vec![response(
            "200 OK",
            &[],
            sample_manifest_body("1.21.9").as_bytes(),
        )])
        .await;
        let source = server.url("/version_manifest_v2.json");
        let manifest = fetch_version_manifest_cached_with(
            &operation,
            std::future::pending(),
            false,
            fetch_manifest_body(&source, &[]),
        )
        .await
        .unwrap();

        assert_eq!(manifest.latest.release, "1.21.9");
        assert_eq!(
            read_persistent_manifest_cache(&operation)
                .expect("read refreshed cache")
                .latest
                .release,
            "1.21.9"
        );
        assert_eq!(server.finish().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    fn sample_manifest_body(release: &str) -> String {
        sample_manifest_body_with_sha1(release, "abc123")
    }

    fn sample_manifest_body_with_sha1(release: &str, sha1: &str) -> String {
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
      "sha1": "{sha1}",
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
        crate::test_temp_root().join(format!(
            "axial-manifest-cache-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn test_library(path: &Path) -> (ManagedLibraryRoot, ManagedLibraryOperation) {
        fs::create_dir_all(path).expect("create managed library root");
        let root = ManagedLibraryRoot::open_for_test(path).expect("open managed library root");
        let operation = root
            .try_acquire()
            .expect("acquire managed library operation");
        operation
            .prepare_layout()
            .expect("prepare managed library layout");
        (root, operation)
    }
}
