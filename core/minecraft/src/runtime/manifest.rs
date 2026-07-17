use super::model::JavaRuntimeLookupError;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

pub(super) const RUNTIME_MANIFEST_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
pub(super) const MAX_RUNTIME_MANIFEST_BYTES: u64 = 16 << 20;
pub(crate) const COMPONENT_MANIFEST_PROOF_FILE: &str = ".axial-runtime-manifest.json";
const RUNTIME_MANIFEST_CONNECT_TIMEOUT_SECS: u64 = 10;
const RUNTIME_MANIFEST_READ_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Copy)]
enum RuntimeSourceTransportPolicy {
    HttpsOnly,
    #[cfg(test)]
    AllowHttpForTest,
}

impl RuntimeSourceTransportPolicy {
    fn requires_https(self) -> bool {
        match self {
            Self::HttpsOnly => true,
            #[cfg(test)]
            Self::AllowHttpForTest => false,
        }
    }
}

fn runtime_source_url_is_secure(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

async fn fetch_bounded_runtime_bytes(
    url: &str,
    policy: RuntimeSourceTransportPolicy,
) -> Result<Vec<u8>, JavaRuntimeLookupError> {
    if policy.requires_https() && !runtime_source_url_is_secure(url) {
        return Err(JavaRuntimeLookupError::Source(
            "runtime source must use HTTPS".to_string(),
        ));
    }
    let response = runtime_manifest_client()
        .get(url)
        .send()
        .await
        .map_err(|error| JavaRuntimeLookupError::Source(error.to_string()))?;
    if policy.requires_https() && response.url().scheme() != "https" {
        return Err(JavaRuntimeLookupError::Source(
            "runtime source redirected to an insecure URL".to_string(),
        ));
    }
    let status = response.status();
    if !status.is_success() {
        return Err(JavaRuntimeLookupError::Source(format!("HTTP {status}")));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_RUNTIME_MANIFEST_BYTES)
    {
        return Err(JavaRuntimeLookupError::Source(
            "runtime manifest response too large".to_string(),
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| JavaRuntimeLookupError::Source(error.to_string()))?;
        if body.len() as u64 + chunk.len() as u64 > MAX_RUNTIME_MANIFEST_BYTES {
            return Err(JavaRuntimeLookupError::Source(
                "runtime manifest response too large".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

pub(super) async fn acquire_runtime_source(
    component: &super::model::RuntimeId,
    primary_platform: &str,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let catalog_bytes = fetch_bounded_runtime_bytes(
        RUNTIME_MANIFEST_URL,
        RuntimeSourceTransportPolicy::HttpsOnly,
    )
    .await?;
    let catalog = serde_json::from_slice::<RuntimeManifest>(&catalog_bytes)
        .map_err(|error| JavaRuntimeLookupError::Source(error.to_string()))?;
    let expected = select_runtime_manifest(&catalog, component, primary_platform)?.clone();
    acquire_runtime_source_from_descriptor(
        component.clone(),
        expected,
        RuntimeSourceTransportPolicy::HttpsOnly,
    )
    .await
}

async fn acquire_runtime_source_from_descriptor(
    component: super::model::RuntimeId,
    expected: RuntimeDownloadManifest,
    policy: RuntimeSourceTransportPolicy,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let bytes = fetch_bounded_runtime_bytes(&expected.url, policy).await?;
    authenticate_runtime_source_bytes(component, expected, bytes)
}

fn authenticate_runtime_source_bytes(
    component: super::model::RuntimeId,
    expected: RuntimeDownloadManifest,
    bytes: Vec<u8>,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    verify_component_manifest_bytes(&bytes, &expected)?;
    let manifest = serde_json::from_slice::<ComponentManifest>(&bytes)
        .map_err(|error| JavaRuntimeLookupError::Source(error.to_string()))?;

    Ok(RuntimeSourceReceipt {
        component,
        bytes: Arc::from(bytes),
        expected,
        manifest,
    })
}

fn verify_component_manifest_bytes(
    bytes: &[u8],
    expected: &RuntimeDownloadManifest,
) -> Result<(), JavaRuntimeLookupError> {
    if bytes.len() as u64 != expected.size {
        return Err(JavaRuntimeLookupError::Source(
            "runtime component manifest size mismatch".to_string(),
        ));
    }
    if expected.sha1.len() != 40 || !expected.sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(JavaRuntimeLookupError::Source(
            "runtime component manifest has invalid checksum proof".to_string(),
        ));
    }
    let actual_sha1 = format!("{:x}", Sha1::digest(bytes));
    if !actual_sha1.eq_ignore_ascii_case(&expected.sha1) {
        return Err(JavaRuntimeLookupError::Source(
            "runtime component manifest checksum mismatch".to_string(),
        ));
    }
    Ok(())
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
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    attempt.error("runtime source redirect limit exceeded")
                } else if attempt.url().scheme() == "https" {
                    attempt.follow()
                } else {
                    attempt.error("runtime source redirect must use HTTPS")
                }
            }))
            .user_agent("axial/0.3")
            .build()
            .expect("runtime manifest HTTP client configuration should be valid")
    })
}
#[derive(Debug, Deserialize)]
pub(super) struct RuntimeManifestEntry {
    pub(super) manifest: RuntimeDownloadManifest,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(super) struct RuntimeDownloadManifest {
    pub(super) url: String,
    pub(super) sha1: String,
    pub(super) size: u64,
}

pub(super) type RuntimeManifest = HashMap<String, HashMap<String, Vec<RuntimeManifestEntry>>>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RuntimeSourceReceipt {
    component: super::model::RuntimeId,
    bytes: Arc<[u8]>,
    expected: RuntimeDownloadManifest,
    manifest: ComponentManifest,
}

impl RuntimeSourceReceipt {
    pub(crate) fn component(&self) -> &super::model::RuntimeId {
        &self.component
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn expected_sha1(&self) -> &str {
        &self.expected.sha1
    }

    pub(crate) fn expected_size(&self) -> u64 {
        self.expected.size
    }

    pub(super) fn manifest(&self) -> &ComponentManifest {
        &self.manifest
    }
}

pub(super) fn select_runtime_manifest<'a>(
    all_runtimes: &'a RuntimeManifest,
    component: &super::model::RuntimeId,
    primary_platform: &str,
) -> Result<&'a RuntimeDownloadManifest, JavaRuntimeLookupError> {
    std::iter::once(primary_platform)
        .chain(
            super::layout::runtime_platform_fallbacks(primary_platform)
                .iter()
                .copied(),
        )
        .find_map(|platform| {
            all_runtimes
                .get(platform)?
                .get(component.as_str())?
                .first()
                .map(|entry| &entry.manifest)
        })
        .ok_or_else(|| JavaRuntimeLookupError::UnsupportedPlatform {
            component: component.as_str().to_string(),
            platform: primary_platform.to_string(),
        })
}

#[cfg(test)]
pub(super) async fn acquire_runtime_source_for_test(
    component: super::model::RuntimeId,
    expected: RuntimeDownloadManifest,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    acquire_runtime_source_from_descriptor(
        component,
        expected,
        RuntimeSourceTransportPolicy::AllowHttpForTest,
    )
    .await
}

#[cfg(test)]
pub(super) fn authenticated_runtime_source_fixture_for_test(
    component: super::model::RuntimeId,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    const MANIFEST: &[u8] = br#"{"files":{"bin":{"type":"directory"},"bin/java":{"type":"file","executable":true,"downloads":{"raw":{"url":"https://fixtures.invalid/java","sha1":"11f6ad8ec52a2984abaafd7c3b516503785c2072","size":1}}}}}"#;
    const MANIFEST_SHA1: &str = "2797f4f6a71abbbf22d8a8d4386f93135e46cf06";

    authenticate_runtime_source_bytes(
        component,
        RuntimeDownloadManifest {
            url: "https://fixtures.invalid/runtime-manifest.json".to_string(),
            sha1: MANIFEST_SHA1.to_string(),
            size: MANIFEST.len() as u64,
        },
        MANIFEST.to_vec(),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn authenticated_runtime_source_from_manifest_for_test(
    component: super::model::RuntimeId,
    manifest: ComponentManifest,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let bytes = serde_json::to_vec(&manifest)
        .map_err(|error| JavaRuntimeLookupError::Source(error.to_string()))?;
    authenticate_runtime_source_bytes(
        component,
        RuntimeDownloadManifest {
            url: "https://fixtures.invalid/runtime-manifest.json".to_string(),
            sha1: format!("{:x}", Sha1::digest(&bytes)),
            size: bytes.len() as u64,
        },
        bytes,
    )
}

#[cfg(feature = "test-support")]
pub(super) fn authenticated_runtime_rebuild_fixture_source(
    component: super::model::RuntimeId,
    java_url: String,
    java_bytes: &[u8],
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let java_relative_path = super::layout::runtime_java_relative_path().to_string();
    let manifest = ComponentManifest {
        files: HashMap::from([(
            java_relative_path,
            ComponentManifestFile {
                kind: "file".to_string(),
                executable: true,
                downloads: Some(ComponentManifestDownloads {
                    raw: Some(ComponentManifestDownload {
                        url: java_url,
                        sha1: Some(format!("{:x}", Sha1::digest(java_bytes))),
                        size: Some(java_bytes.len() as u64),
                    }),
                    lzma: None,
                }),
                target: None,
            },
        )]),
    };
    let bytes = serde_json::to_vec(&manifest)
        .map_err(|error| JavaRuntimeLookupError::Source(error.to_string()))?;
    authenticate_runtime_source_bytes(
        component,
        RuntimeDownloadManifest {
            url: "https://fixtures.invalid/runtime-manifest.json".to_string(),
            sha1: format!("{:x}", Sha1::digest(&bytes)),
            size: bytes.len() as u64,
        },
        bytes,
    )
}

#[cfg(test)]
pub(super) async fn fetch_runtime_manifest_bytes_for_test(
    url: &str,
) -> Result<Vec<u8>, JavaRuntimeLookupError> {
    fetch_bounded_runtime_bytes(url, RuntimeSourceTransportPolicy::AllowHttpForTest).await
}

#[cfg(test)]
pub(super) fn runtime_source_url_is_secure_for_test(url: &str) -> bool {
    runtime_source_url_is_secure(url)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ComponentManifest {
    pub(crate) files: HashMap<String, ComponentManifestFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ComponentManifestDownloads {
    #[serde(default)]
    pub(crate) raw: Option<ComponentManifestDownload>,
    #[serde(default)]
    pub(crate) lzma: Option<ComponentManifestDownload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
