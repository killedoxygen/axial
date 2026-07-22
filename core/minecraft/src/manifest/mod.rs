use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::managed_fs::{ManagedDir, ManagedFileGuard, ManagedLibraryOperation};

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const MANIFEST_CACHE_NAME: &str = "version_manifest_v2.json";
const CACHE_TTL: Duration = Duration::from_secs(600);
const MAX_MANIFEST_BYTES: u64 = 8 << 20;
const MANIFEST_CLIENT_MAX_IDLE_PER_HOST: usize = 4;
const MANIFEST_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 60;
const MANIFEST_CLIENT_TCP_KEEPALIVE_SECS: u64 = 60;
const MANIFEST_RETRY_DELAY_MILLIS: [u64; 3] = [500, 1_500, 4_000];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManifestFetchFailureKind {
    Network,
    Interrupted,
    Http(u16),
    InsecureRedirect,
    TooLarge,
}

struct ManifestFetchFailure {
    kind: ManifestFetchFailureKind,
    message: String,
}

impl ManifestFetchFailure {
    fn new(kind: ManifestFetchFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self.kind,
            ManifestFetchFailureKind::Network
                | ManifestFetchFailureKind::Interrupted
                | ManifestFetchFailureKind::Http(408 | 429 | 500..=599)
        )
    }
}

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

pub(crate) async fn fetch_registered_repair_version_manifest(
    version_id: &str,
    expected_metadata_sha1: &str,
) -> Result<VersionManifest, String> {
    let (manifest, fetched_live) = resolve_registered_repair_manifest(
        fresh_cached_manifest(),
        stale_cached_manifest(),
        version_id,
        expected_metadata_sha1,
        fetch_manifest_live(),
    )
    .await?;
    if fetched_live {
        update_manifest_cache(manifest.clone());
    }
    Ok(manifest)
}

async fn resolve_registered_repair_manifest<F>(
    fresh_manifest: Option<VersionManifest>,
    stale_manifest: Option<VersionManifest>,
    version_id: &str,
    expected_metadata_sha1: &str,
    live_manifest: F,
) -> Result<(VersionManifest, bool), String>
where
    F: Future<Output = Result<VersionManifest, String>>,
{
    if let Some(manifest) = fresh_manifest.filter(|manifest| {
        manifest_contains_registered_repair_entry(manifest, version_id, expected_metadata_sha1)
    }) {
        return Ok((manifest, false));
    }
    let stale_manifest = stale_manifest.filter(|manifest| {
        manifest_contains_registered_repair_entry(manifest, version_id, expected_metadata_sha1)
    });
    resolve_read_only_manifest(live_manifest.await, stale_manifest)
}

fn manifest_contains_registered_repair_entry(
    manifest: &VersionManifest,
    version_id: &str,
    expected_metadata_sha1: &str,
) -> bool {
    manifest
        .versions
        .iter()
        .find(|entry| entry.id == version_id)
        .is_some_and(|entry| {
            !entry.url.trim().is_empty() && entry.sha1.eq_ignore_ascii_case(expected_metadata_sha1)
        })
}

fn resolve_read_only_manifest(
    live_result: Result<VersionManifest, String>,
    stale_manifest: Option<VersionManifest>,
) -> Result<(VersionManifest, bool), String> {
    match live_result {
        Ok(manifest) => Ok((manifest, true)),
        Err(error) => stale_manifest
            .map(|manifest| (manifest, false))
            .ok_or(error),
    }
}

pub(crate) async fn fetch_fresh_install_version_manifest() -> Result<VersionManifest, String> {
    fetch_manifest_live().await
}

pub async fn fetch_version_manifest_cached(
    operation: &ManagedLibraryOperation,
) -> Result<VersionManifest, String> {
    fetch_version_manifest_cached_from_url(operation, MANIFEST_URL).await
}

async fn fetch_version_manifest_cached_from_url(
    operation: &ManagedLibraryOperation,
    manifest_url: &str,
) -> Result<VersionManifest, String> {
    if let Some(value) = fresh_persistent_manifest_cache(operation) {
        update_manifest_cache(value.clone());
        return Ok(value);
    }

    if let Some(value) = fresh_cached_manifest() {
        let _ = write_persistent_manifest_cache_value(operation, &value).await;
        return Ok(value);
    }

    let stale = read_persistent_manifest_cache(operation)
        .ok()
        .or_else(stale_cached_manifest);
    if let Some(stale) = stale {
        return Ok(refresh_stale_manifest(operation, manifest_url, stale).await);
    }

    let (manifest, live_body) = resolve_manifest_from_live_or_cache(
        operation,
        fetch_manifest_live_body_with_policy(
            manifest_url,
            manifest_url == MANIFEST_URL,
            manifest_url == MANIFEST_URL,
        )
        .await,
        None,
    )?;

    if let Some(live_body) = live_body {
        let _ = write_persistent_manifest_cache(operation, &live_body).await;
    }

    update_manifest_cache(manifest.clone());
    Ok(manifest)
}

async fn refresh_stale_manifest(
    operation: &ManagedLibraryOperation,
    manifest_url: &str,
    stale: VersionManifest,
) -> VersionManifest {
    let Ok(body) = fetch_manifest_live_body_with_policy(
        manifest_url,
        manifest_url == MANIFEST_URL,
        manifest_url == MANIFEST_URL,
    )
    .await
    else {
        return stale;
    };
    let Ok(manifest) = parse_manifest_body(&body) else {
        return stale;
    };
    let _ = write_persistent_manifest_cache(operation, &body).await;
    update_manifest_cache(manifest.clone());
    manifest
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

fn fresh_persistent_manifest_cache(
    operation: &ManagedLibraryOperation,
) -> Option<VersionManifest> {
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
    fetch_manifest_live_body_with_policy(MANIFEST_URL, true, true).await
}

#[cfg(test)]
async fn fetch_manifest_live_body_from_url(url: &str) -> Result<Vec<u8>, String> {
    fetch_manifest_live_body_with_policy(url, false, false).await
}

async fn fetch_manifest_live_body_with_policy(
    url: &str,
    require_https: bool,
    retry_transient: bool,
) -> Result<Vec<u8>, String> {
    fetch_manifest_live_body_with_retry_delays(
        url,
        require_https,
        retry_transient,
        &MANIFEST_RETRY_DELAY_MILLIS,
    )
    .await
}

async fn fetch_manifest_live_body_with_retry_delays(
    url: &str,
    require_https: bool,
    retry_transient: bool,
    retry_delays: &[u64],
) -> Result<Vec<u8>, String> {
    let mut retry_delays = retry_delays.iter().copied();
    loop {
        match fetch_manifest_live_body_attempt(url, require_https).await {
            Ok(body) => return Ok(body),
            Err(failure) => {
                let Some(delay_millis) = retry_delays
                    .next()
                    .filter(|_| retry_transient && failure.retryable())
                else {
                    return Err(failure.message);
                };
                tokio::time::sleep(Duration::from_millis(delay_millis)).await;
            }
        }
    }
}

async fn fetch_manifest_live_body_attempt(
    url: &str,
    require_https: bool,
) -> Result<Vec<u8>, ManifestFetchFailure> {
    let response = manifest_client().get(url).send().await.map_err(|error| {
        let kind = if error.is_redirect() {
            ManifestFetchFailureKind::InsecureRedirect
        } else if error.is_timeout() {
            ManifestFetchFailureKind::Interrupted
        } else {
            ManifestFetchFailureKind::Network
        };
        ManifestFetchFailure::new(kind, format!("fetching version manifest: {error}"))
    })?;
    if require_https && response.url().scheme() != "https" {
        return Err(ManifestFetchFailure::new(
            ManifestFetchFailureKind::InsecureRedirect,
            "fetching version manifest: insecure redirect",
        ));
    }
    let status = response.status();
    if !status.is_success() {
        return Err(ManifestFetchFailure::new(
            ManifestFetchFailureKind::Http(status.as_u16()),
            format!("fetching version manifest: HTTP {status}"),
        ));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_MANIFEST_BYTES)
    {
        return Err(ManifestFetchFailure::new(
            ManifestFetchFailureKind::TooLarge,
            "reading version manifest: response too large",
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            let kind = if error.is_timeout() {
                ManifestFetchFailureKind::Interrupted
            } else {
                ManifestFetchFailureKind::Network
            };
            ManifestFetchFailure::new(kind, format!("reading version manifest: {error}"))
        })?;
        if body.len() as u64 + chunk.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ManifestFetchFailure::new(
                ManifestFetchFailureKind::TooLarge,
                "reading version manifest: response too large",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

fn manifest_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("axial/0.3")
            .pool_max_idle_per_host(MANIFEST_CLIENT_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(Duration::from_secs(MANIFEST_CLIENT_POOL_IDLE_TIMEOUT_SECS))
            .tcp_keepalive(Duration::from_secs(MANIFEST_CLIENT_TCP_KEEPALIVE_SECS))
            .build()
            .expect("version manifest HTTP client configuration should be valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_fs::ManagedLibraryRoot;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

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
    async fn registered_repair_manifest_resolution_is_read_only_and_path_independent() {
        const VERSION_ID: &str = "1.21.11";
        const PINNED_SHA1: &str = "1111111111111111111111111111111111111111";
        let root = temp_dir("registered-repair-read-only");
        fs::create_dir_all(&root).expect("create test root");
        let sentinel = root.join("persistent-sentinel");
        fs::write(&sentinel, b"persistent sentinel").expect("write persistent sentinel");
        let mut insufficient = parse_manifest_body(sample_manifest_body(VERSION_ID).as_bytes())
            .expect("insufficient cached manifest");
        let mut duplicate = insufficient.versions[0].clone();
        duplicate.sha1 = PINNED_SHA1.to_string();
        insufficient.versions.push(duplicate);
        let server = ScriptedManifestServer::start(vec![(
            200,
            sample_manifest_body_with_sha1(VERSION_ID, PINNED_SHA1),
        )]);
        let url = server.url();

        let (resolved, fetched_live) = resolve_registered_repair_manifest(
            Some(insufficient),
            None,
            VERSION_ID,
            PINNED_SHA1,
            async move {
                let body =
                    fetch_manifest_live_body_with_retry_delays(&url, false, true, &[0, 0, 0])
                        .await?;
                parse_manifest_body(&body)
            },
        )
        .await
        .expect("insufficient memory manifest falls back to live discovery");

        assert_eq!(resolved.latest.release, VERSION_ID);
        assert!(fetched_live);
        assert_eq!(server.join(), 1);
        assert_eq!(
            fs::read(&sentinel).expect("read persistent sentinel"),
            b"persistent sentinel"
        );

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
        let stale = read_persistent_manifest_cache(&operation).expect("read stale cache");
        let manifest = refresh_stale_manifest(
            &operation,
            "http://127.0.0.1:9/version_manifest_v2.json",
            stale,
        )
        .await;

        assert_eq!(manifest.latest.release, "1.21.8");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_persistent_cache_is_replaced_by_synchronous_refresh() {
        let root = temp_dir("manifest-cache-synchronous-refresh");
        let (_library, operation) = test_library(&root);
        write_persistent_manifest_cache(&operation, sample_manifest_body("1.21.8").as_bytes())
            .await
            .expect("write cache");
        let stale = read_persistent_manifest_cache(&operation).expect("read stale cache");
        let server = TestManifestServer::start(200, sample_manifest_body("1.21.9"));

        let manifest = refresh_stale_manifest(&operation, &server.url(), stale).await;

        assert_eq!(manifest.latest.release, "1.21.9");
        assert_eq!(
            read_persistent_manifest_cache(&operation)
                .expect("read refreshed cache")
                .latest
                .release,
            "1.21.9"
        );
        server.join();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn live_manifest_fetch_reads_async_http_body() {
        let server = TestManifestServer::start(200, sample_manifest_body("1.21.7"));

        let body = fetch_manifest_live_body_from_url(&server.url())
            .await
            .expect("fetch live manifest body");
        let manifest = parse_manifest_body(&body).expect("parse fetched manifest body");

        assert_eq!(manifest.latest.release, "1.21.7");
        assert_eq!(manifest.versions[0].id, "1.21.7");
        server.join();
    }

    #[tokio::test]
    async fn live_manifest_fetch_rejects_http_errors() {
        let server = TestManifestServer::start(503, "unavailable".to_string());

        let error = fetch_manifest_live_body_from_url(&server.url())
            .await
            .expect_err("HTTP error should fail");

        assert!(error.contains("HTTP 503"), "{error}");
        server.join();
    }

    #[test]
    fn manifest_fetch_retry_policy_is_typed_and_bounded() {
        for kind in [
            ManifestFetchFailureKind::Network,
            ManifestFetchFailureKind::Interrupted,
            ManifestFetchFailureKind::Http(408),
            ManifestFetchFailureKind::Http(429),
            ManifestFetchFailureKind::Http(500),
            ManifestFetchFailureKind::Http(599),
        ] {
            assert!(ManifestFetchFailure::new(kind, "transient").retryable());
        }
        for kind in [
            ManifestFetchFailureKind::Http(400),
            ManifestFetchFailureKind::Http(404),
            ManifestFetchFailureKind::Http(600),
            ManifestFetchFailureKind::InsecureRedirect,
            ManifestFetchFailureKind::TooLarge,
        ] {
            assert!(!ManifestFetchFailure::new(kind, "terminal").retryable());
        }
        assert_eq!(MANIFEST_RETRY_DELAY_MILLIS, [500, 1_500, 4_000]);
    }

    #[tokio::test]
    async fn canonical_manifest_retry_executes_only_typed_transient_attempts() {
        let expected = sample_manifest_body("1.21.10");
        let server = ScriptedManifestServer::start(vec![
            (503, "unavailable".to_string()),
            (429, "rate limited".to_string()),
            (200, expected.clone()),
        ]);

        let body =
            fetch_manifest_live_body_with_retry_delays(&server.url(), false, true, &[0, 0, 0])
                .await
                .expect("transient manifest attempts reach success");

        assert_eq!(body, expected.as_bytes());
        assert_eq!(server.join(), 3);

        let terminal = ScriptedManifestServer::start(vec![(404, "missing".to_string())]);
        let error =
            fetch_manifest_live_body_with_retry_delays(&terminal.url(), false, true, &[0, 0, 0])
                .await
                .expect_err("terminal manifest response must not retry");
        assert!(error.contains("HTTP 404"), "{error}");
        assert_eq!(terminal.join(), 1);

        let redirect = ScriptedManifestServer::start(vec![(302, String::new())]);
        let error =
            fetch_manifest_live_body_with_retry_delays(&redirect.url(), false, true, &[0, 0, 0])
                .await
                .expect_err("redirect response must not be followed or retried");
        assert!(error.contains("HTTP 302"), "{error}");
        assert_eq!(redirect.join(), 1);
    }

    #[tokio::test]
    async fn live_manifest_fetch_rejects_oversized_content_length() {
        let server = TestManifestServer::start_with_content_length(
            200,
            "ignored".to_string(),
            MAX_MANIFEST_BYTES + 1,
        );

        let error = fetch_manifest_live_body_from_url(&server.url())
            .await
            .expect_err("oversized manifest should fail");

        assert_eq!(error, "reading version manifest: response too large");
        server.join();
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

    struct TestManifestServer {
        address: String,
        server: thread::JoinHandle<()>,
    }

    struct ScriptedManifestServer {
        address: String,
        requests: Arc<AtomicUsize>,
        server: thread::JoinHandle<()>,
    }

    impl ScriptedManifestServer {
        fn start(responses: Vec<(u16, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted manifest server");
            let address = listener
                .local_addr()
                .expect("scripted manifest server addr");
            let requests = Arc::new(AtomicUsize::new(0));
            let observed_requests = requests.clone();
            let server = thread::spawn(move || {
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().expect("accept scripted request");
                    let mut request = [0_u8; 1024];
                    let _ = stream.read(&mut request);
                    observed_requests.fetch_add(1, Ordering::SeqCst);
                    write_response(&mut stream, status, &body, body.len() as u64);
                }
            });
            Self {
                address: address.to_string(),
                requests,
                server,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/version_manifest_v2.json", self.address)
        }

        fn join(self) -> usize {
            self.server.join().expect("scripted manifest server join");
            self.requests.load(Ordering::SeqCst)
        }
    }

    impl TestManifestServer {
        fn start(status: u16, body: String) -> Self {
            Self::start_with_content_length(status, body.clone(), body.len() as u64)
        }

        fn start_with_content_length(status: u16, body: String, content_length: u64) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind manifest test server");
            let address = listener.local_addr().expect("manifest test server addr");
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept manifest request");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                write_response(&mut stream, status, &body, content_length);
            });

            Self {
                address: address.to_string(),
                server,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/version_manifest_v2.json", self.address)
        }

        fn join(self) {
            self.server.join().expect("manifest test server join");
        }
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &str, content_length: u64) {
        let reason = if status == 200 { "OK" } else { "Error" };
        let location = if (300..400).contains(&status) {
            "Location: /redirected\r\n"
        } else {
            ""
        };
        let headers = format!(
            "HTTP/1.1 {status} {reason}\r\n{location}Content-Length: {content_length}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(headers.as_bytes())
            .expect("write response headers");
        stream
            .write_all(body.as_bytes())
            .expect("write response body");
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-manifest-cache-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn test_library(path: &Path) -> (ManagedLibraryRoot, ManagedLibraryOperation) {
        fs::create_dir_all(path).expect("create managed library root");
        let root = ManagedLibraryRoot::open_for_test(path).expect("open managed library root");
        let operation = root.try_acquire().expect("acquire managed library operation");
        operation.prepare_layout().expect("prepare managed library layout");
        (root, operation)
    }
}
