use super::facts::{
    emit_execution_download_facts, execution_download_fact, fetch_failure_fact,
    integrity_mismatch_fact, metadata_facts,
};
use super::integrity::is_sha1_hex;
use super::model::{
    DownloadError, DownloadIntegrityError, ExecutionDownloadFact, ExecutionDownloadFactKind,
    ExpectedIntegrity, SelectedDownloadArtifactKind,
};
use crate::http;
use hex::FromHex as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const MAX_PROCESSOR_MAPPINGS_BYTES: u64 = 64 << 20;

pub(crate) async fn acquire_processor_mappings(
    base: &crate::launch::VersionJson,
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    let (entry, expected) = processor_mappings_contract(base)?;
    acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
        client: http::client(),
        kind: SelectedDownloadArtifactKind::ClientMappings,
        url: &entry.url,
        logical_identity: &base.id,
        expected: &expected,
        max_bytes: entry.size as usize,
        target: "minecraft_client_mappings",
        fact_tx: None,
    })
    .await
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestProcessorMappingsTransport {
    provider_url: String,
    loopback_url: String,
}

#[cfg(test)]
impl TestProcessorMappingsTransport {
    pub(crate) fn new(provider_url: String, loopback_url: String) -> Self {
        let url = reqwest::Url::parse(&loopback_url).expect("mapping fixture transport URL");
        assert_eq!(url.scheme(), "http");
        assert!(url.host_str().is_some_and(|host| {
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        }));
        assert!(url.port().is_some());
        assert!(url.username().is_empty() && url.password().is_none());
        assert!(url.fragment().is_none());
        Self {
            provider_url,
            loopback_url,
        }
    }
}

#[cfg(test)]
pub(crate) async fn acquire_test_processor_mappings(
    base: &crate::launch::VersionJson,
    transport: &TestProcessorMappingsTransport,
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    let (entry, expected) = processor_mappings_contract(base)?;
    if entry.url != transport.provider_url {
        return Err(source_artifact_metadata_error(
            "minecraft_client_mappings",
            "invalid",
        ));
    }
    let mut source =
        acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
            client: http::loopback_client(),
            kind: SelectedDownloadArtifactKind::ClientMappings,
            url: &transport.loopback_url,
            logical_identity: &base.id,
            expected: &expected,
            max_bytes: entry.size as usize,
            target: "minecraft_client_mappings",
            fact_tx: None,
        })
        .await?;
    // Only transport is substituted; the retained HTTPS declaration supplied every integrity bound.
    source.provider_url = entry.url.clone();
    Ok(source)
}

fn processor_mappings_contract(
    base: &crate::launch::VersionJson,
) -> Result<(&crate::launch::DownloadEntry, ExpectedIntegrity), DownloadError> {
    let invalid = || source_artifact_metadata_error("minecraft_client_mappings", "invalid");
    let entry = base
        .downloads
        .client_mappings
        .as_ref()
        .ok_or_else(invalid)?;
    let size = u64::try_from(entry.size).map_err(|_| invalid())?;
    let url = reqwest::Url::parse(&entry.url).map_err(|_| invalid())?;
    if base.id.is_empty()
        || size == 0
        || size > MAX_PROCESSOR_MAPPINGS_BYTES
        || !is_sha1_hex(&entry.sha1)
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok((
        entry,
        ExpectedIntegrity::from_mojang(entry.size, &entry.sha1),
    ))
}

pub(crate) struct AuthenticatedSelectedArtifactSource {
    bytes: Arc<[u8]>,
    observed_size: u64,
    observed_sha1: [u8; 20],
    kind: SelectedDownloadArtifactKind,
    provider_url: String,
    logical_identity: String,
    expected: ExpectedIntegrity,
}

pub(crate) struct AuthenticatedSelectedArtifactVersionBundleParts {
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) observed_size: u64,
    pub(crate) observed_sha1: [u8; 20],
    pub(crate) kind: SelectedDownloadArtifactKind,
    pub(crate) logical_identity: String,
    pub(crate) expected: ExpectedIntegrity,
}

impl AuthenticatedSelectedArtifactSource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn kind(&self) -> SelectedDownloadArtifactKind {
        self.kind
    }

    pub(crate) fn provider_url(&self) -> &str {
        &self.provider_url
    }

    pub(crate) fn logical_identity(&self) -> &str {
        &self.logical_identity
    }

    pub(crate) fn expected(&self) -> &ExpectedIntegrity {
        &self.expected
    }

    pub(crate) fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    pub(crate) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    pub(crate) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    pub(crate) fn into_version_bundle_parts(
        self,
    ) -> AuthenticatedSelectedArtifactVersionBundleParts {
        AuthenticatedSelectedArtifactVersionBundleParts {
            bytes: self.bytes,
            observed_size: self.observed_size,
            observed_sha1: self.observed_sha1,
            kind: self.kind,
            logical_identity: self.logical_identity,
            expected: self.expected,
        }
    }
}

pub(super) struct SelectedArtifactSourceRequest<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) kind: SelectedDownloadArtifactKind,
    pub(super) url: &'a str,
    pub(super) logical_identity: &'a str,
    pub(super) expected: &'a ExpectedIntegrity,
    pub(super) max_bytes: usize,
    pub(super) target: &'a str,
    pub(super) fact_tx: Option<&'a mpsc::UnboundedSender<ExecutionDownloadFact>>,
}

pub(super) async fn acquire_authenticated_selected_artifact_source(
    request: SelectedArtifactSourceRequest<'_>,
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    acquire_with_retry_delays(request, &crate::http::RETRY_DELAYS).await
}

async fn acquire_with_retry_delays(
    request: SelectedArtifactSourceRequest<'_>,
    retry_delays: &[Duration],
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    let Some(expected_sha1) = request.expected.sha1.as_deref() else {
        emit_source_metadata_failure(
            request.expected,
            request.target,
            request.fact_tx,
            ExecutionDownloadFactKind::MetadataMissing,
            "sha1",
        );
        return Err(source_artifact_metadata_error(request.target, "missing"));
    };
    let Ok(sha1) = <[u8; 20]>::from_hex(expected_sha1) else {
        emit_source_metadata_failure(
            request.expected,
            request.target,
            request.fact_tx,
            ExecutionDownloadFactKind::MetadataInvalid,
            "sha1",
        );
        return Err(source_artifact_metadata_error(request.target, "invalid"));
    };
    let expected = http::Expected {
        size: request.expected.size,
        sha1: Some(sha1),
    };
    let fetched = http::retry(
        retry_delays,
        || async {
            let result = http::fetch_once(
                request.client,
                request.url,
                request.max_bytes as u64,
                expected,
            )
            .await;
            if let Err(error) = &result {
                let fact = match integrity_error(&request, error) {
                    Some(integrity) => integrity_mismatch_fact(request.target, &integrity),
                    None => fetch_failure_fact(request.target, error),
                };
                emit_execution_download_facts(request.fact_tx, &[fact]);
            }
            result
        },
        http::FetchError::is_retryable,
    )
    .await
    .map_err(|error| fetch_download_error(&request, error))?;
    Ok(AuthenticatedSelectedArtifactSource {
        observed_size: fetched.bytes.len() as u64,
        observed_sha1: fetched.sha1,
        bytes: Arc::from(fetched.bytes),
        kind: request.kind,
        provider_url: request.url.to_string(),
        logical_identity: request.logical_identity.to_string(),
        expected: request.expected.clone(),
    })
}

#[cfg(test)]
pub(super) async fn acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
    client: &reqwest::Client,
    url: &str,
    expected: &ExpectedIntegrity,
    max_bytes: usize,
    target: &str,
    retry_delays: &[Duration],
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    acquire_with_retry_delays(
        SelectedArtifactSourceRequest {
            client,
            kind: SelectedDownloadArtifactKind::VersionJson,
            url,
            logical_identity: target,
            expected,
            max_bytes,
            target,
            fact_tx: None,
        },
        retry_delays,
    )
    .await
}

fn integrity_error(
    request: &SelectedArtifactSourceRequest<'_>,
    error: &http::FetchError,
) -> Option<DownloadIntegrityError> {
    match error {
        http::FetchError::SizeMismatch { expected, observed } => {
            Some(DownloadIntegrityError::SizeMismatch {
                file: request.target.to_string(),
                expected: *expected,
                actual: *observed,
            })
        }
        http::FetchError::DigestMismatch { observed } => {
            Some(DownloadIntegrityError::Sha1Mismatch {
                file: request.target.to_string(),
                expected: request.expected.sha1.clone().unwrap_or_default(),
                actual: hex::encode(observed),
            })
        }
        _ => None,
    }
}

fn fetch_download_error(
    request: &SelectedArtifactSourceRequest<'_>,
    error: http::FetchError,
) -> DownloadError {
    if let Some(integrity) = integrity_error(request, &error) {
        return DownloadError::Integrity(integrity.to_string());
    }
    match error {
        http::FetchError::Request(error) => match reqwest::Url::parse(request.url) {
            Ok(url) => DownloadError::Request(error.with_url(url)),
            Err(_) => DownloadError::Request(error),
        },
        http::FetchError::TooLarge { .. } | http::FetchError::Allocation => {
            DownloadError::Integrity(format!(
                "{} exceeds the safe in-memory size limit",
                request.target
            ))
        }
        error => DownloadError::Integrity(format!("{}: {error}", request.target)),
    }
}

fn emit_source_metadata_failure(
    expected: &ExpectedIntegrity,
    target: &str,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    fact_kind: ExecutionDownloadFactKind,
    field: &str,
) {
    let Some(fact_tx) = fact_tx else {
        return;
    };
    let mut facts = metadata_facts(expected, target);
    if !facts.iter().any(|fact| fact.kind == fact_kind) {
        facts.push(execution_download_fact(
            fact_kind,
            target,
            vec![("field", field)],
        ));
    }
    emit_execution_download_facts(Some(fact_tx), &facts);
}

fn source_artifact_metadata_error(target: &str, status: &str) -> DownloadError {
    DownloadError::Integrity(format!("{target} integrity metadata is {status}"))
}

#[cfg(test)]
mod processor_mappings_tests {
    use super::*;
    use crate::http::test_server::{response, serve};
    use sha1::{Digest as _, Sha1};

    fn version() -> crate::launch::VersionJson {
        serde_json::from_value(serde_json::json!({
            "id": "1.20.1",
            "downloads": {
                "client": {"size": 1, "sha1": "0".repeat(40), "url": "https://example.invalid/client"},
                "client_mappings": {
                    "size": 3, "sha1": format!("{:x}", Sha1::digest(b"map")),
                    "url": "https://example.invalid/client-mappings.txt"
                }
            }
        })).unwrap()
    }

    #[test]
    fn processor_mappings_use_the_retained_client_mapping_contract() {
        let base = version();
        let (entry, expected) = processor_mappings_contract(&base).unwrap();
        assert_eq!(entry, base.downloads.client_mappings.as_ref().unwrap());
        assert_eq!(expected.size, Some(3));
        assert_eq!(expected.sha1, Some(format!("{:x}", Sha1::digest(b"map"))));
    }

    #[tokio::test]
    async fn processor_mappings_reject_invalid_metadata_before_network() {
        let source = serde_json::to_value(version()).unwrap();
        for (field, value) in [
            ("size", serde_json::json!(0)),
            ("size", serde_json::json!(-1)),
            ("size", serde_json::json!(MAX_PROCESSOR_MAPPINGS_BYTES + 1)),
            ("sha1", serde_json::json!("")),
            ("sha1", serde_json::json!("z".repeat(40))),
            ("url", serde_json::json!("http://127.0.0.1:1/mappings")),
            ("url", serde_json::json!("file:///mappings")),
            (
                "url",
                serde_json::json!("https://user:password@example.invalid/mappings"),
            ),
            (
                "url",
                serde_json::json!("https://example.invalid/mappings#fragment"),
            ),
        ] {
            let mut changed = source.clone();
            changed["downloads"]["client_mappings"][field] = value;
            let base = serde_json::from_value(changed).unwrap();
            assert!(matches!(
                acquire_processor_mappings(&base).await,
                Err(DownloadError::Integrity(_))
            ));
            let transport = TestProcessorMappingsTransport::new(
                base.downloads.client_mappings.as_ref().unwrap().url.clone(),
                "http://127.0.0.1:1/mapping".to_string(),
            );
            assert!(matches!(
                acquire_test_processor_mappings(&base, &transport).await,
                Err(DownloadError::Integrity(_))
            ));
        }
        let mut missing = version();
        missing.downloads.client_mappings = None;
        assert!(matches!(
            acquire_processor_mappings(&missing).await,
            Err(DownloadError::Integrity(_))
        ));
    }

    #[tokio::test]
    async fn processor_mapping_transfer_retains_exact_bytes_and_rejects_digest_drift() {
        for bytes in [b"map".as_slice(), b"bad".as_slice(), b"maps".as_slice()] {
            let server = serve(vec![response("200 OK", &[], bytes)]).await;
            let base = version();
            let (_, expected) = processor_mappings_contract(&base).unwrap();
            let mut transport = TestProcessorMappingsTransport::new(
                "https://example.invalid/other-mappings.txt".to_string(),
                server.url("/mapping"),
            );
            assert!(matches!(
                acquire_test_processor_mappings(&base, &transport).await,
                Err(DownloadError::Integrity(_))
            ));
            transport.provider_url = base.downloads.client_mappings.as_ref().unwrap().url.clone();
            let result = acquire_test_processor_mappings(&base, &transport).await;
            assert_eq!(server.finish().len(), 1);
            if bytes == b"map" {
                let source = result.unwrap();
                assert_eq!(source.bytes(), bytes);
                assert_eq!(source.kind(), SelectedDownloadArtifactKind::ClientMappings);
                assert_eq!(source.logical_identity(), base.id);
                assert_eq!(source.expected(), &expected);
                assert_eq!(source.provider_url(), transport.provider_url);
            } else {
                assert!(matches!(result, Err(DownloadError::Integrity(_))));
            }
        }
    }
}
