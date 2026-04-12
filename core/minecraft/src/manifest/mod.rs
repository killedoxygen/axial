use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
        return Ok(value.clone());
    }

    let cached_stale = cache.lock().ok().and_then(|cache| cache.value.clone());
    let manifest = match fetch_manifest_live().await {
        Ok(manifest) => manifest,
        Err(error) => {
            if let Some(stale) = cached_stale {
                return Ok(stale);
            }
            return Err(error);
        }
    };

    if let Ok(mut cache) = cache.lock() {
        cache.value = Some(manifest.clone());
        cache.fetched_at = Some(Instant::now());
    }

    Ok(manifest)
}

async fn fetch_manifest_live() -> Result<VersionManifest, String> {
    tokio::task::spawn_blocking(|| {
        let response = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(15))
            .timeout_write(Duration::from_secs(15))
            .user_agent("croopor/0.3")
            .build()
            .get(MANIFEST_URL)
            .call()
            .map_err(|error| format!("fetching version manifest: {error}"))?;

        let mut reader = response.into_reader();
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .map_err(|error| format!("reading version manifest: {error}"))?;
        serde_json::from_slice::<VersionManifest>(&body)
            .map_err(|error| format!("parsing version manifest: {error}"))
    })
    .await
    .map_err(|error| format!("fetching version manifest: {error}"))?
}
