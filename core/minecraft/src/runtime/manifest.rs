use super::model::JavaRuntimeLookupError;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

pub(super) const RUNTIME_MANIFEST_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
pub(super) const MAX_RUNTIME_MANIFEST_BYTES: u64 = 16 << 20;
pub(crate) const COMPONENT_MANIFEST_PROOF_FILE: &str = ".axial-runtime-manifest.json";
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
            .user_agent("axial/0.3")
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
pub(crate) struct ComponentManifest {
    pub(crate) files: HashMap<String, ComponentManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ComponentManifestFile {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[cfg_attr(not(unix), allow(dead_code))]
    #[serde(default)]
    pub(crate) executable: bool,
    #[serde(default)]
    pub(crate) downloads: Option<ComponentManifestDownloads>,
    #[serde(default)]
    pub(crate) target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ComponentManifestDownloads {
    #[serde(default)]
    pub(crate) raw: Option<ComponentManifestDownload>,
    #[serde(default)]
    pub(crate) lzma: Option<ComponentManifestDownload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ComponentManifestDownload {
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) sha1: Option<String>,
    #[serde(default)]
    pub(crate) size: Option<u64>,
}

pub(crate) fn component_manifest_proof_bytes(
    manifest: &ComponentManifest,
) -> Result<Vec<u8>, serde_json::Error> {
    #[derive(Serialize)]
    struct CanonicalManifest<'a> {
        files: BTreeMap<&'a str, &'a ComponentManifestFile>,
    }

    serde_json::to_vec_pretty(&CanonicalManifest {
        files: manifest
            .files
            .iter()
            .map(|(path, file)| (path.as_str(), file))
            .collect(),
    })
}
