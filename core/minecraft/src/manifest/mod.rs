use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::download::{
    DownloadError, ExecutionDownloadError, write_launcher_managed_artifact_bytes_to_temp,
};
use crate::paths::version_manifest_cache_path;

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const CACHE_TTL: Duration = Duration::from_secs(600);
const MAX_MANIFEST_BYTES: u64 = 8 << 20;
const MANIFEST_CLIENT_MAX_IDLE_PER_HOST: usize = 4;
const MANIFEST_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 60;
const MANIFEST_CLIENT_TCP_KEEPALIVE_SECS: u64 = 60;

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
    fetch_version_manifest_cached_from_url(library_dir, MANIFEST_URL).await
}

async fn fetch_version_manifest_cached_from_url(
    library_dir: &Path,
    manifest_url: &str,
) -> Result<VersionManifest, String> {
    let cache_path = version_manifest_cache_path(library_dir);

    if let Some(value) = fresh_persistent_manifest_cache(&cache_path) {
        update_manifest_cache(value.clone());
        return Ok(value);
    }

    if let Some(value) = fresh_cached_manifest() {
        let _ = write_persistent_manifest_cache_value(&cache_path, &value).await;
        return Ok(value);
    }

    // Serve any stale manifest immediately and refresh in the background:
    // callers on the interactive path (the create dialog) must not wait on
    // the network once a manifest exists locally.
    let stale = read_persistent_manifest_cache(&cache_path)
        .ok()
        .or_else(stale_cached_manifest);
    if let Some(stale) = stale {
        spawn_manifest_refresh(cache_path, manifest_url.to_string());
        return Ok(stale);
    }

    let (manifest, live_body) = resolve_manifest_from_live_or_cache(
        &cache_path,
        fetch_manifest_live_body_from_url(manifest_url).await,
        None,
    )?;

    if let Some(live_body) = live_body {
        let _ = write_persistent_manifest_cache(&cache_path, &live_body).await;
    }

    update_manifest_cache(manifest.clone());
    Ok(manifest)
}

fn spawn_manifest_refresh(cache_path: PathBuf, manifest_url: String) {
    static REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if REFRESH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        if let Ok(body) = fetch_manifest_live_body_from_url(&manifest_url).await
            && let Ok(manifest) = parse_manifest_body(&body)
        {
            let _ = write_persistent_manifest_cache(&cache_path, &body).await;
            update_manifest_cache(manifest);
        }
        REFRESH_IN_FLIGHT.store(false, Ordering::Release);
    });
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

fn fresh_persistent_manifest_cache(path: &Path) -> Option<VersionManifest> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    if modified.elapsed().ok()? >= CACHE_TTL {
        return None;
    }
    read_persistent_manifest_cache(path).ok()
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
    fetch_manifest_live_body_from_url(MANIFEST_URL).await
}

async fn fetch_manifest_live_body_from_url(url: &str) -> Result<Vec<u8>, String> {
    let response = manifest_client()
        .get(url)
        .send()
        .await
        .map_err(|error| format!("fetching version manifest: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("fetching version manifest: HTTP {status}"));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_MANIFEST_BYTES)
    {
        return Err("reading version manifest: response too large".to_string());
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("reading version manifest: {error}"))?;
        if body.len() as u64 + chunk.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("reading version manifest: response too large".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

async fn write_persistent_manifest_cache(path: &Path, data: &[u8]) -> Result<(), String> {
    parse_manifest_body(data)?;
    let tmp_path = manifest_cache_tmp_path(path);
    write_launcher_managed_artifact_bytes_to_temp(path, &tmp_path, data)
        .await
        .map(|_| ())
        .map_err(manifest_execution_download_error)
}

async fn write_persistent_manifest_cache_value(
    path: &Path,
    manifest: &VersionManifest,
) -> Result<(), String> {
    let data = serde_json::to_vec(manifest)
        .map_err(|error| format!("serializing version manifest cache: {error}"))?;
    write_persistent_manifest_cache(path, &data).await
}

fn manifest_execution_download_error(error: ExecutionDownloadError) -> String {
    let context = match error.kind {
        crate::download::ExecutionDownloadFactKind::PromoteFailed => {
            "promoting version manifest cache"
        }
        crate::download::ExecutionDownloadFactKind::PermissionFailure
        | crate::download::ExecutionDownloadFactKind::TempWriteFailed => {
            "writing version manifest cache"
        }
        _ => "writing version manifest cache",
    };
    match error.into_download_error() {
        DownloadError::FileOperation(error) => format!("{context}: {error}"),
        error => format!("{context}: {error}"),
    }
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

fn manifest_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(15))
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    #[tokio::test]
    async fn writes_and_reads_persistent_manifest_cache() {
        let root = temp_dir("manifest-cache-round-trip");
        let cache_path = root.join("cache").join("version_manifest_v2.json");
        let body = sample_manifest_body("1.21.5");

        write_persistent_manifest_cache(&cache_path, body.as_bytes())
            .await
            .expect("write cache");
        let manifest = read_persistent_manifest_cache(&cache_path).expect("read cache");

        assert_eq!(manifest.latest.release, "1.21.5");
        assert_eq!(manifest.versions[0].id, "1.21.5");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fallback_returns_cached_manifest_when_live_provider_fails() {
        let root = temp_dir("manifest-cache-fallback");
        let cache_path = root.join("version_manifest_v2.json");
        write_persistent_manifest_cache(&cache_path, sample_manifest_body("1.21.4").as_bytes())
            .await
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

    #[tokio::test]
    async fn fresh_manifest_value_can_seed_a_requested_library_cache() {
        let root = temp_dir("manifest-cache-fresh-library");
        let cache_path = root.join("cache").join("version_manifest_v2.json");
        let manifest =
            parse_manifest_body(sample_manifest_body("1.21.6").as_bytes()).expect("parse manifest");

        write_persistent_manifest_cache_value(&cache_path, &manifest)
            .await
            .expect("write cache value");

        let cached = read_persistent_manifest_cache(&cache_path).expect("read cache");
        assert_eq!(cached.latest.release, "1.21.6");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_persistent_manifest_is_preferred_for_requested_library() {
        let root = temp_dir("manifest-cache-fresh-local-first");
        let cache_path = version_manifest_cache_path(&root);
        write_persistent_manifest_cache(&cache_path, sample_manifest_body("1.21.7").as_bytes())
            .await
            .expect("write cache");

        let manifest = fresh_persistent_manifest_cache(&cache_path).expect("fresh local manifest");

        assert_eq!(manifest.latest.release, "1.21.7");
        assert_eq!(manifest.versions[0].id, "1.21.7");

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

    #[tokio::test]
    async fn live_parse_error_can_fall_back_to_cached_manifest() {
        let root = temp_dir("manifest-cache-live-parse");
        let cache_path = root.join("version_manifest_v2.json");
        write_persistent_manifest_cache(&cache_path, sample_manifest_body("1.21.3").as_bytes())
            .await
            .expect("write cache");

        let (manifest, live_body) =
            resolve_manifest_from_live_or_cache(&cache_path, Ok(b"not json".to_vec()), None)
                .expect("cached manifest should satisfy live parse failure");

        assert_eq!(manifest.latest.release, "1.21.3");
        assert!(live_body.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn expired_persistent_cache_is_served_stale_without_waiting_on_the_network() {
        let root = temp_dir("manifest-cache-swr");
        let cache_path = version_manifest_cache_path(&root);
        write_persistent_manifest_cache(&cache_path, sample_manifest_body("1.21.8").as_bytes())
            .await
            .expect("write cache");
        let cache_file = fs::OpenOptions::new()
            .write(true)
            .open(&cache_path)
            .expect("open cache");
        cache_file
            .set_modified(SystemTime::now() - CACHE_TTL - Duration::from_secs(60))
            .expect("backdate cache");
        let manifest = fetch_version_manifest_cached_from_url(
            &root,
            "http://127.0.0.1:9/version_manifest_v2.json",
        )
        .await
        .expect("stale manifest should be served");

        assert_eq!(manifest.latest.release, "1.21.8");

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

    #[tokio::test]
    async fn shared_promotion_replaces_existing_manifest_cache() {
        let root = temp_dir("manifest-cache-promote");
        fs::create_dir_all(&root).expect("create root");
        let tmp_path = root.join("version_manifest_v2.tmp");
        let cache_path = root.join("version_manifest_v2.json");
        fs::write(&cache_path, b"old").expect("write old cache");
        fs::write(&tmp_path, b"new").expect("write temp cache");

        crate::download::promote_launcher_managed_artifact_temp_once(&tmp_path, &cache_path)
            .await
            .expect("promote temp cache");

        assert_eq!(fs::read(&cache_path).expect("read promoted cache"), b"new");
        assert!(!tmp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shared_promotion_preserves_destination_when_temp_is_missing() {
        let root = temp_dir("manifest-cache-missing-temp");
        fs::create_dir_all(&root).expect("create root");
        let tmp_path = root.join("missing.tmp");
        let cache_path = root.join("version_manifest_v2.json");
        fs::write(&cache_path, b"old").expect("write old cache");

        let error =
            crate::download::promote_launcher_managed_artifact_temp_once(&tmp_path, &cache_path)
                .await
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

    struct TestManifestServer {
        address: String,
        server: thread::JoinHandle<()>,
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
        let headers = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
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
}
