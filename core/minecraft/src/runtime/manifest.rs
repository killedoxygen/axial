use super::model::JavaRuntimeLookupError;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

pub(super) const RUNTIME_MANIFEST_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
pub(super) const MAX_RUNTIME_MANIFEST_BYTES: u64 = 16 << 20;
pub(super) const COMPONENT_MANIFEST_PROOF_FILE: &str = ".croopor-runtime-manifest.json";
const RUNTIME_MANIFEST_CONNECT_TIMEOUT_SECS: u64 = 10;
const RUNTIME_MANIFEST_READ_TIMEOUT_SECS: u64 = 30;

pub(super) async fn fetch_runtime_json<T>(url: &str) -> Result<T, JavaRuntimeLookupError>
where
    T: serde::de::DeserializeOwned,
{
    let response = runtime_manifest_client()
        .get(url)
        .send()
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(JavaRuntimeLookupError::Download(format!("HTTP {status}")));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_RUNTIME_MANIFEST_BYTES)
    {
        return Err(JavaRuntimeLookupError::Download(
            "runtime manifest response too large".to_string(),
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        if body.len() as u64 + chunk.len() as u64 > MAX_RUNTIME_MANIFEST_BYTES {
            return Err(JavaRuntimeLookupError::Download(
                "runtime manifest response too large".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice::<T>(&body)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

pub(super) fn runtime_manifest_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(
                RUNTIME_MANIFEST_CONNECT_TIMEOUT_SECS,
            ))
            .read_timeout(std::time::Duration::from_secs(
                RUNTIME_MANIFEST_READ_TIMEOUT_SECS,
            ))
            .user_agent("croopor/0.3")
            .build()
            .expect("runtime manifest HTTP client configuration should be valid")
    })
}
#[derive(Debug, Deserialize)]
pub(super) struct RuntimeManifestEntry {
    pub(super) manifest: RuntimeDownloadManifest,
}

#[derive(Debug, Deserialize)]
pub(super) struct RuntimeDownloadManifest {
    pub(super) url: String,
}

pub(super) type RuntimeManifest = HashMap<String, HashMap<String, Vec<RuntimeManifestEntry>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ComponentManifest {
    pub(super) files: HashMap<String, ComponentManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ComponentManifestFile {
    #[serde(rename = "type")]
    pub(super) kind: String,
    #[cfg_attr(not(unix), allow(dead_code))]
    #[serde(default)]
    pub(super) executable: bool,
    #[serde(default)]
    pub(super) downloads: Option<ComponentManifestDownloads>,
    #[serde(default)]
    pub(super) target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ComponentManifestDownloads {
    #[serde(default)]
    pub(super) raw: Option<ComponentManifestDownload>,
    #[serde(default)]
    pub(super) lzma: Option<ComponentManifestDownload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ComponentManifestDownload {
    pub(super) url: String,
    #[serde(default)]
    pub(super) sha1: Option<String>,
    #[serde(default)]
    pub(super) size: Option<u64>,
}
