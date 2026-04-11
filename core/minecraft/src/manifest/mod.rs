use serde::{Deserialize, Serialize};
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

pub async fn fetch_version_manifest() -> Result<VersionManifest, reqwest::Error> {
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

    let response = reqwest::Client::new()
        .get(MANIFEST_URL)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let manifest = response
        .error_for_status()?
        .json::<VersionManifest>()
        .await?;

    if let Ok(mut cache) = cache.lock() {
        cache.value = Some(manifest.clone());
        cache.fetched_at = Some(Instant::now());
    }

    Ok(manifest)
}
