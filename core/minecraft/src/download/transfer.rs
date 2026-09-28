use super::facts::{
    emit_execution_download_facts, execution_download_fact, integrity_mismatch_fact,
    metadata_facts, no_download_fact_fields, size_mismatch_fact,
};
use super::integrity::is_sha1_hex;
use super::model::{
    DownloadError, DownloadIntegrityError, ExecutionDownloadFact, ExecutionDownloadFactKind,
    ExpectedIntegrity, SelectedDownloadArtifactKind,
};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const DOWNLOAD_RETRY_DELAY_MILLIS: [u64; 3] = [500, 1_500, 4_000];
const MAX_PROCESSOR_MAPPINGS_BYTES: u64 = 64 << 20;

pub(crate) async fn acquire_processor_mappings(
    base: &crate::launch::VersionJson,
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    let (entry, expected) = processor_mappings_contract(base)?;
    let client = reqwest::Client::builder()
        .user_agent("axial/0.3")
        .https_only(true)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;
    acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
        client: &client,
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
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut source =
        acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
            client: &client,
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
    let retry_delays = default_download_retry_delays();
    acquire_authenticated_selected_artifact_source_with_retry_delays(request, &retry_delays).await
}

async fn acquire_authenticated_selected_artifact_source_with_retry_delays(
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
    if !is_sha1_hex(expected_sha1) {
        emit_source_metadata_failure(
            request.expected,
            request.target,
            request.fact_tx,
            ExecutionDownloadFactKind::MetadataInvalid,
            "sha1",
        );
        return Err(source_artifact_metadata_error(request.target, "invalid"));
    }

    let max_bytes = u64::try_from(request.max_bytes).unwrap_or(u64::MAX);
    let mut next_delay = 0_usize;
    loop {
        match acquire_authenticated_selected_artifact_source_attempt(&request, max_bytes).await {
            Ok(source) => return Ok(source),
            Err(error) if error.retryable && next_delay < retry_delays.len() => {
                let delay = retry_delays[next_delay];
                next_delay += 1;
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error.error),
        }
    }
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
    acquire_authenticated_selected_artifact_source_with_retry_delays(
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

struct VerifiedSelectedBytesAttemptError {
    error: DownloadError,
    retryable: bool,
}

async fn acquire_authenticated_selected_artifact_source_attempt(
    request: &SelectedArtifactSourceRequest<'_>,
    max_bytes: u64,
) -> Result<AuthenticatedSelectedArtifactSource, VerifiedSelectedBytesAttemptError> {
    let response = request
        .client
        .get(request.url)
        .send()
        .await
        .map_err(|error| {
            emit_execution_download_facts(
                request.fact_tx,
                &[execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    request.target,
                    no_download_fact_fields(),
                )],
            );
            VerifiedSelectedBytesAttemptError {
                error: DownloadError::Request(error),
                retryable: true,
            }
        })?;
    if let Err(error) = response.error_for_status_ref() {
        let status = response.status();
        emit_execution_download_facts(
            request.fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::ProviderFailure,
                request.target,
                vec![("status", status.as_u16().to_string())],
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Request(error),
            retryable: is_retryable_provider_status(status.as_u16()),
        });
    }
    let declared_content_length = response.content_length();
    if declared_content_length.is_some_and(|length| length > max_bytes) {
        emit_execution_download_facts(
            request.fact_tx,
            &[size_mismatch_fact(
                request.target,
                max_bytes,
                declared_content_length.unwrap_or(0),
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(format!(
                "{} exceeds the safe in-memory size limit",
                request.target
            )),
            retryable: false,
        });
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            emit_execution_download_facts(
                request.fact_tx,
                &[execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    request.target,
                    no_download_fact_fields(),
                )],
            );
            VerifiedSelectedBytesAttemptError {
                error: DownloadError::Request(error),
                retryable: true,
            }
        })?;
        let next_len = body.len().saturating_add(chunk.len());
        if u64::try_from(next_len).unwrap_or(u64::MAX) > max_bytes {
            emit_execution_download_facts(
                request.fact_tx,
                &[size_mismatch_fact(
                    request.target,
                    max_bytes,
                    u64::try_from(next_len).unwrap_or(u64::MAX),
                )],
            );
            return Err(VerifiedSelectedBytesAttemptError {
                error: DownloadError::Integrity(format!(
                    "{} exceeds the safe in-memory size limit",
                    request.target
                )),
                retryable: false,
            });
        }
        body.extend_from_slice(&chunk);
    }
    if declared_content_length.is_some_and(|length| length != body.len() as u64) {
        emit_execution_download_facts(
            request.fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::Interrupted,
                request.target,
                no_download_fact_fields(),
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(format!(
                "{} download ended before the declared content length",
                request.target
            )),
            retryable: true,
        });
    }

    let observed_size = body.len() as u64;
    let observed_sha1: [u8; 20] = Sha1::digest(&body).into();
    if let Err(error) = verify_source_integrity(
        request.target,
        request.expected,
        observed_size,
        &observed_sha1,
    ) {
        emit_execution_download_facts(
            request.fact_tx,
            &[integrity_mismatch_fact(request.target, &error)],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(error.to_string()),
            retryable: false,
        });
    }
    Ok(AuthenticatedSelectedArtifactSource {
        bytes: Arc::from(body),
        observed_size,
        observed_sha1,
        kind: request.kind,
        provider_url: request.url.to_string(),
        logical_identity: request.logical_identity.to_string(),
        expected: request.expected.clone(),
    })
}

fn verify_source_integrity(
    target: &str,
    expected: &ExpectedIntegrity,
    observed_size: u64,
    observed_sha1: &[u8; 20],
) -> Result<(), DownloadIntegrityError> {
    if let Some(expected_size) = expected.size
        && observed_size != expected_size
    {
        return Err(DownloadIntegrityError::SizeMismatch {
            file: target.to_string(),
            expected: expected_size,
            actual: observed_size,
        });
    }
    let expected_sha1 = expected
        .sha1
        .as_deref()
        .expect("source acquisition validates expected sha1 first");
    let actual_sha1 = observed_sha1
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
        return Err(DownloadIntegrityError::Sha1Mismatch {
            file: target.to_string(),
            expected: expected_sha1.to_string(),
            actual: actual_sha1,
        });
    }
    Ok(())
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

fn default_download_retry_delays() -> [Duration; 3] {
    DOWNLOAD_RETRY_DELAY_MILLIS.map(Duration::from_millis)
}

fn is_retryable_provider_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

#[cfg(test)]
mod processor_mappings_tests {
    use super::*;

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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for bytes in [b"map".as_slice(), b"bad".as_slice(), b"maps".as_slice()] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mapping", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                stream.read(&mut request).await.unwrap();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.write_all(bytes).await.unwrap();
            });
            let base = version();
            let (_, expected) = processor_mappings_contract(&base).unwrap();
            let mut transport = TestProcessorMappingsTransport::new(
                "https://example.invalid/other-mappings.txt".to_string(),
                url,
            );
            assert!(matches!(
                acquire_test_processor_mappings(&base, &transport).await,
                Err(DownloadError::Integrity(_))
            ));
            transport.provider_url = base.downloads.client_mappings.as_ref().unwrap().url.clone();
            let result = acquire_test_processor_mappings(&base, &transport).await;
            server.await.unwrap();
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
