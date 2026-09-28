use std::io::Write;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::tasks::CancellationToken;

use super::*;

const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

struct Fixture {
    origin: String,
    task: JoinHandle<Vec<Vec<u8>>>,
}

impl Fixture {
    async fn serve(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(3), listener.accept())
                        .await
                        .expect("fixture was not requested")
                        .unwrap();
                requests.push(read_request(&mut stream).await);
                // A rejected body may close the connection before all test bytes
                // are written. The result assertion establishes the rejection.
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
            requests
        });
        Self { origin, task }
    }

    fn request(&self, limits: ResponseLimits, integrity: IntegrityPolicy) -> DownloadRequest {
        DownloadRequest::get(
            format!("{}/artifact?token=private-query", self.origin),
            OriginPolicy::loopback_for_tests([&self.origin], 3).unwrap(),
            limits,
            integrity,
        )
    }

    async fn finish(mut self) -> Vec<Vec<u8>> {
        (&mut self.task).await.unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "request ended before its body was complete");
            bytes.extend_from_slice(&buffer[..read]);
            assert!(bytes.len() <= 2 * 1024 * 1024);
            if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map(|length| length.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    return bytes;
                }
            }
        }
    })
    .await
    .expect("fixture request did not finish")
}

fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn client() -> ProviderClient {
    ProviderClient::new(ClientConfig {
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(2),
        total_timeout: Duration::from_secs(3),
        ..ClientConfig::default()
    })
    .unwrap()
}

fn public_request(url: &str) -> DownloadRequest {
    let origin = reqwest::Url::parse(url)
        .unwrap()
        .origin()
        .ascii_serialization();
    DownloadRequest::get(
        url,
        OriginPolicy::https([origin], 3).unwrap(),
        bounded(),
        unhashed(),
    )
}

#[tokio::test]
async fn public_fetch_rejects_private_mixed_empty_and_wrong_port_dns_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let url = format!(
        "https://archive.invalid:{}/pack?secret=hidden",
        local.port()
    );
    let public = std::net::SocketAddr::from(([1, 1, 1, 1], local.port()));
    for addresses in [
        vec![local],
        vec![public, local],
        Vec::new(),
        vec![std::net::SocketAddr::from(([1, 1, 1, 1], 0))],
    ] {
        let error = client()
            .fetch_public_resolved(
                public_request(&url),
                &CancellationToken::new(),
                |host, port| {
                    assert_eq!(host, "archive.invalid");
                    assert_eq!(port, local.port());
                    std::future::ready(Ok(addresses.clone()))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, DownloadError::OriginNotAllowed);
        assert!(!error.to_string().contains("hidden"));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn public_fetch_rejects_literal_loopback_without_invoking_dns_or_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let request = public_request(&format!("https://{}/pack", listener.local_addr().unwrap()));
    let error = client()
        .fetch_public_resolved(request, &CancellationToken::new(), |_, _| {
            panic!("literal IPs must not invoke DNS");
            #[allow(unreachable_code)]
            std::future::ready(Ok(Vec::new()))
        })
        .await
        .unwrap_err();
    assert_eq!(error, DownloadError::OriginNotAllowed);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn public_fetch_checks_origin_and_body_policy_before_dns() {
    for (request, expected) in [
        (
            DownloadRequest::get(
                "https://other.invalid/pack",
                OriginPolicy::https(["https://archive.invalid"], 3).unwrap(),
                bounded(),
                unhashed(),
            ),
            DownloadError::OriginNotAllowed,
        ),
        (
            DownloadRequest::get(
                "https://archive.invalid/pack",
                OriginPolicy::https(["https://archive.invalid"], 3).unwrap(),
                ResponseLimits::new(0, 100),
                unhashed(),
            ),
            DownloadError::InvalidPolicy,
        ),
    ] {
        assert_eq!(
            client()
                .fetch_public_resolved(request, &CancellationToken::new(), |_, _| {
                    panic!("invalid request must not invoke DNS");
                    #[allow(unreachable_code)]
                    std::future::ready(Ok(Vec::new()))
                })
                .await
                .unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn public_fetch_cancellation_drops_in_flight_dns() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct ResolverGuard<'a>(&'a AtomicBool);
    impl Drop for ResolverGuard<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let started = tokio::sync::Notify::new();
    let dropped = AtomicBool::new(false);
    let cancel = CancellationToken::new();
    let client = client();
    let fetch = client.fetch_public_resolved(
        public_request("https://archive.invalid/pack"),
        &cancel,
        |_, _| async {
            let _guard = ResolverGuard(&dropped);
            started.notify_one();
            std::future::pending().await
        },
    );
    let trigger = async {
        started.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(fetch, trigger);
    assert_eq!(result.unwrap_err(), DownloadError::Cancelled);
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn public_fetch_total_deadline_includes_dns_and_preserves_retryable_errors() {
    let client = ProviderClient::new(ClientConfig {
        total_timeout: Duration::from_millis(20),
        ..ClientConfig::default()
    })
    .unwrap();
    assert_eq!(
        client
            .fetch_public_resolved(
                public_request("https://archive.invalid/pack"),
                &CancellationToken::new(),
                |_, _| std::future::pending()
            )
            .await
            .unwrap_err(),
        DownloadError::Timeout
    );
    assert_eq!(
        client
            .fetch_public_resolved(
                public_request("https://archive.invalid/pack"),
                &CancellationToken::new(),
                |_, _| std::future::ready(Err(std::io::Error::from(
                    std::io::ErrorKind::ConnectionRefused
                )))
            )
            .await
            .unwrap_err(),
        DownloadError::Network
    );
}

#[tokio::test]
async fn public_dns_admission_resolves_once_and_deduplicates_a_bounded_address_set() {
    let url = reqwest::Url::parse("https://archive.invalid:8443/pack").unwrap();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let addresses = super::managed::public_addresses(&url, |host, port| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(host, "archive.invalid");
        assert_eq!(port, 8443);
        std::future::ready(Ok((1..=64)
            .flat_map(|last| [std::net::SocketAddr::from(([1, 1, 1, last], port)); 2])
            .collect()))
    })
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        addresses,
        (1..=32)
            .map(|last| std::net::SocketAddr::from(([1, 1, 1, last], 8443)))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn public_literal_addresses_keep_their_exact_address_and_port_without_dns() {
    for source in [
        "https://1.1.1.1:8443/pack",
        "https://[2606:4700:4700::1111]:8443/pack",
    ] {
        let url = reqwest::Url::parse(source).unwrap();
        let addresses = super::managed::public_addresses(&url, |_, _| {
            panic!("literal IPs must not invoke DNS");
            #[allow(unreachable_code)]
            std::future::ready(Ok(Vec::new()))
        })
        .await
        .unwrap();
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].port(), 8443);
        assert_eq!(
            Some(addresses[0].ip()),
            match url.host().unwrap() {
                url::Host::Ipv4(ip) => Some(ip.into()),
                url::Host::Ipv6(ip) => Some(ip.into()),
                _ => None,
            }
        );
    }
}

fn unhashed() -> IntegrityPolicy {
    IntegrityPolicy::Unhashed {
        reason: UnhashedReason::ProviderMetadata,
        expected_size: None,
    }
}

fn bounded() -> ResponseLimits {
    ResponseLimits::new(1024 * 1024, 1024 * 1024)
}

async fn fetch_response(
    bytes: Vec<u8>,
    limits: ResponseLimits,
) -> Result<DownloadedBytes, DownloadError> {
    let fixture = Fixture::serve(vec![bytes]).await;
    let result = client()
        .fetch(
            fixture.request(limits, unhashed()),
            &CancellationToken::new(),
        )
        .await;
    fixture.finish().await;
    result
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn production_policy_requires_explicit_https_origins() {
    let policy =
        OriginPolicy::https(["https://example.com", "https://cdn.example.com:8443"], 3).unwrap();
    assert!(
        policy
            .admit("https://EXAMPLE.com:443/artifact?signature=abc")
            .is_ok()
    );
    assert!(
        policy
            .admit("https://cdn.example.com:8443/artifact")
            .is_ok()
    );
    for source in [
        "https://other.example.com/a",
        "https://example.com.evil.test/a",
        "https://example.com:8443/a",
    ] {
        assert_eq!(
            policy.admit(source).unwrap_err(),
            DownloadError::OriginNotAllowed
        );
    }
    for source in [
        "http://example.com/a",
        "http://127.0.0.1:3000/a",
        "ftp://example.com/a",
    ] {
        assert_eq!(
            policy.admit(source).unwrap_err(),
            DownloadError::InsecureOrigin
        );
    }
}

#[test]
fn policy_rejects_credentials_fragments_and_unbounded_origin_sets() {
    let policy = OriginPolicy::https(["https://example.com"], 3).unwrap();
    for source in [
        "https://user@example.com/a",
        "https://user:password@example.com/a",
        "https://example.com/a#fragment",
        "not a URL",
    ] {
        assert_eq!(policy.admit(source).unwrap_err(), DownloadError::InvalidUrl);
    }
    assert_eq!(
        policy
            .admit(&format!("https://example.com/{}", "a".repeat(16 * 1024)))
            .unwrap_err(),
        DownloadError::InvalidUrl
    );
    for source in [
        "https://example.com/path",
        "https://example.com/?query=yes",
        "https://example.com/#fragment",
    ] {
        assert!(OriginPolicy::https([source], 3).is_err());
    }
    assert!(OriginPolicy::https(Vec::<String>::new(), 3).is_err());
    assert!(OriginPolicy::https(["https://example.com"], 11).is_err());
    assert!(
        OriginPolicy::https(
            (0..65).map(|index| format!("https://host{index}.example.com")),
            3
        )
        .is_err()
    );
}

#[test]
fn http_test_seam_only_admits_literal_loopback_and_exact_ports() {
    let policy =
        OriginPolicy::loopback_for_tests(["http://127.0.0.1:3000", "http://[::1]:3000"], 0)
            .unwrap();
    assert!(policy.admit("http://127.0.0.1:3000/a").is_ok());
    assert!(policy.admit("http://[::1]:3000/a").is_ok());
    assert_eq!(
        policy.admit("http://127.0.0.1:3001/a").unwrap_err(),
        DownloadError::OriginNotAllowed
    );
    for source in [
        "http://localhost:3000",
        "http://192.168.1.1:3000",
        "http://example.com:3000",
    ] {
        assert_eq!(
            OriginPolicy::loopback_for_tests([source], 0).unwrap_err(),
            DownloadError::InsecureOrigin
        );
    }
}

#[test]
fn checksum_validation_requires_exact_hex_width() {
    for (algorithm, length) in [
        (HashAlgorithm::Sha1, 40),
        (HashAlgorithm::Sha256, 64),
        (HashAlgorithm::Sha512, 128),
    ] {
        assert_eq!(
            Checksum::from_hex(algorithm, &"AB".repeat(length / 2))
                .unwrap()
                .algorithm(),
            algorithm
        );
        for value in [
            "a".repeat(length - 1),
            "a".repeat(length + 1),
            "g".repeat(length),
        ] {
            assert_eq!(
                Checksum::from_hex(algorithm, &value).unwrap_err(),
                DownloadError::InvalidChecksum
            );
        }
    }
}

#[test]
fn client_configuration_and_response_limits_are_finite() {
    for config in [
        ClientConfig {
            connect_timeout: Duration::ZERO,
            ..ClientConfig::default()
        },
        ClientConfig {
            read_timeout: Duration::ZERO,
            ..ClientConfig::default()
        },
        ClientConfig {
            total_timeout: Duration::ZERO,
            ..ClientConfig::default()
        },
        ClientConfig {
            user_agent: String::new(),
            ..ClientConfig::default()
        },
        ClientConfig {
            user_agent: "a".repeat(257),
            ..ClientConfig::default()
        },
        ClientConfig {
            user_agent: "bad\r\nheader".into(),
            ..ClientConfig::default()
        },
    ] {
        assert!(matches!(
            ProviderClient::new(config),
            Err(DownloadError::InvalidPolicy)
        ));
    }
    for limits in [
        ResponseLimits::new(0, 1),
        ResponseLimits::new(1, 0),
        ResponseLimits::new(512 * 1024 * 1024 + 1, 1),
        ResponseLimits::new(1, 512 * 1024 * 1024 + 1),
    ] {
        assert_eq!(limits.validate(), Err(DownloadError::InvalidPolicy));
    }
    assert!(
        ResponseLimits::new(512 * 1024 * 1024, 512 * 1024 * 1024)
            .validate()
            .is_ok()
    );
}

#[tokio::test]
async fn complete_body_reports_explicit_unhashed_evidence_and_redacted_origin() {
    let fixture = Fixture::serve(vec![response(200, "", b"abc")]).await;
    let origin = fixture.origin.clone();
    let downloaded = client()
        .fetch(
            fixture.request(ResponseLimits::new(3, 3), unhashed()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(downloaded.bytes(), b"abc");
    assert_eq!(
        downloaded.evidence(),
        &DownloadEvidence {
            encoded_bytes: 3,
            decoded_bytes: 3,
            observed_sha256: ABC_SHA256.into(),
            integrity: IntegrityEvidence::Unhashed {
                reason: UnhashedReason::ProviderMetadata
            },
            redirects: 0,
            final_origin: origin,
        }
    );
    assert!(!format!("{downloaded:?}").contains("private-query"));
    let requests = fixture.finish().await;
    assert!(String::from_utf8_lossy(&requests[0]).contains("accept-encoding: gzip, deflate"));
    assert_eq!(downloaded.into_bytes(), b"abc");
}

#[tokio::test]
async fn provider_hashes_are_verified_against_independent_known_vectors() {
    for (algorithm, expected) in [
        (
            HashAlgorithm::Sha1,
            "a9993e364706816aba3e25717850c26c9cd0d89d",
        ),
        (HashAlgorithm::Sha256, ABC_SHA256),
        (
            HashAlgorithm::Sha512,
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
    ] {
        let fixture = Fixture::serve(vec![response(200, "", b"abc")]).await;
        let policy = IntegrityPolicy::Checksum {
            checksum: Checksum::from_hex(algorithm, expected).unwrap(),
            expected_size: Some(3),
        };
        let downloaded = client()
            .fetch(
                fixture.request(bounded(), policy),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            downloaded.evidence().integrity,
            IntegrityEvidence::ChecksumMatched { algorithm }
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn integrity_mismatch_never_returns_bytes() {
    for (policy, error) in [
        (
            IntegrityPolicy::Checksum {
                checksum: Checksum::from_hex(HashAlgorithm::Sha256, &"00".repeat(32)).unwrap(),
                expected_size: Some(3),
            },
            DownloadError::ChecksumMismatch,
        ),
        (
            IntegrityPolicy::Unhashed {
                reason: UnhashedReason::ProviderDoesNotPublishChecksum,
                expected_size: Some(4),
            },
            DownloadError::SizeMismatch,
        ),
    ] {
        let fixture = Fixture::serve(vec![response(200, "", b"abc")]).await;
        assert_eq!(
            client()
                .fetch(
                    fixture.request(bounded(), policy),
                    &CancellationToken::new()
                )
                .await
                .unwrap_err(),
            error
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn invalid_size_policy_is_rejected_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let request = DownloadRequest::get(
        &origin,
        OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
        ResponseLimits::new(3, 3),
        IntegrityPolicy::Unhashed {
            reason: UnhashedReason::ProviderMetadata,
            expected_size: Some(4),
        },
    );
    assert_eq!(
        client()
            .fetch(request, &CancellationToken::new())
            .await
            .unwrap_err(),
        DownloadError::InvalidPolicy
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn both_declared_and_chunked_transfers_enforce_encoded_limits() {
    let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n".to_vec();
    for bytes in [response(200, "", b"abcd"), chunked] {
        assert_eq!(
            fetch_response(bytes, ResponseLimits::new(3, 10))
                .await
                .unwrap_err(),
            DownloadError::EncodedLimitExceeded { limit: 3 }
        );
    }
}

#[tokio::test]
async fn identity_responses_enforce_decoded_limits() {
    assert_eq!(
        fetch_response(response(200, "", b"abcd"), ResponseLimits::new(10, 3))
            .await
            .unwrap_err(),
        DownloadError::DecodedLimitExceeded { limit: 3 }
    );
}

#[tokio::test]
async fn closed_and_unfinished_chunked_bodies_are_rejected() {
    for bytes in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabc".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n"
            .to_vec(),
    ] {
        assert_eq!(
            fetch_response(bytes, bounded()).await.unwrap_err(),
            DownloadError::IncompleteBody
        );
    }
}

#[tokio::test]
async fn partial_content_and_content_ranges_are_not_complete_artifacts() {
    for bytes in [
        response(206, "", b"abc"),
        response(200, "Content-Range: bytes 0-2/10\r\n", b"abc"),
    ] {
        assert_eq!(
            fetch_response(bytes, bounded()).await.unwrap_err(),
            DownloadError::IncompleteBody
        );
    }
}

#[tokio::test]
async fn http_errors_are_sanitized_and_not_automatically_retried() {
    for status in [400, 401, 404, 429, 500, 503] {
        let result = fetch_response(response(status, "", b"private provider details"), bounded())
            .await
            .unwrap_err();
        assert_eq!(result, DownloadError::HttpStatus { status });
        assert!(!result.to_string().contains("private"));
    }
}

#[tokio::test]
async fn gzip_and_deflate_verify_decoded_content_and_separate_byte_counts() {
    for (encoding, body) in [("gzip", gzip(b"abc")), ("deflate", deflate(b"abc"))] {
        let encoded_length = body.len() as u64;
        let fixture = Fixture::serve(vec![response(
            200,
            &format!("Content-Encoding: {encoding}\r\n"),
            &body,
        )])
        .await;
        let integrity = IntegrityPolicy::Checksum {
            checksum: Checksum::from_hex(HashAlgorithm::Sha256, ABC_SHA256).unwrap(),
            expected_size: Some(3),
        };
        let result = client()
            .fetch(
                fixture.request(bounded(), integrity),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.bytes(), b"abc");
        assert_eq!(result.evidence().encoded_bytes, encoded_length);
        assert_eq!(result.evidence().decoded_bytes, 3);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn compressed_expansion_cannot_exceed_decoded_limit() {
    let payload = vec![b'x'; 160 * 1024];
    for (encoding, body) in [("gzip", gzip(&payload)), ("deflate", deflate(&payload))] {
        let bytes = response(200, &format!("Content-Encoding: {encoding}\r\n"), &body);
        assert_eq!(
            fetch_response(bytes, ResponseLimits::new(16 * 1024, 70 * 1024))
                .await
                .unwrap_err(),
            DownloadError::DecodedLimitExceeded { limit: 70 * 1024 }
        );
    }
}

#[tokio::test]
async fn deflate_larger_than_one_decoder_chunk_remains_valid() {
    let payload = vec![b'x'; 160 * 1024];
    let decoded = fetch_response(
        response(200, "Content-Encoding: deflate\r\n", &deflate(&payload)),
        bounded(),
    )
    .await
    .unwrap();
    assert_eq!(decoded.bytes(), payload);
}

#[tokio::test]
async fn all_gzip_members_contribute_to_decoded_bounds_and_integrity() {
    let mut body = gzip(b"ab");
    body.extend(gzip(b"c"));
    let fixture = Fixture::serve(vec![response(200, "Content-Encoding: gzip\r\n", &body)]).await;
    let integrity = IntegrityPolicy::Checksum {
        checksum: Checksum::from_hex(HashAlgorithm::Sha256, ABC_SHA256).unwrap(),
        expected_size: Some(3),
    };
    let decoded = client()
        .fetch(
            fixture.request(bounded(), integrity),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(decoded.bytes(), b"abc");
    fixture.finish().await;
    assert_eq!(
        fetch_response(
            response(200, "Content-Encoding: gzip\r\n", &body),
            ResponseLimits::new(1024, 2)
        )
        .await
        .unwrap_err(),
        DownloadError::DecodedLimitExceeded { limit: 2 }
    );
}

#[tokio::test]
async fn truncated_corrupt_and_trailing_compressed_data_is_rejected() {
    for (encoding, complete) in [("gzip", gzip(b"abc")), ("deflate", deflate(b"abc"))] {
        let mut corrupt = complete.clone();
        *corrupt.last_mut().unwrap() ^= 0xff;
        let mut trailing = complete.clone();
        trailing.extend_from_slice(b"unexpected trailing payload");
        for body in [
            complete[..complete.len() - 1].to_vec(),
            corrupt,
            trailing,
            Vec::new(),
        ] {
            assert_eq!(
                fetch_response(
                    response(200, &format!("Content-Encoding: {encoding}\r\n"), &body),
                    bounded()
                )
                .await
                .unwrap_err(),
                DownloadError::InvalidEncoding,
                "encoding {encoding}"
            );
        }
    }
}

#[tokio::test]
async fn unsupported_or_layered_encodings_are_rejected() {
    for headers in [
        "Content-Encoding: br\r\n",
        "Content-Encoding: gzip, deflate\r\n",
        "Content-Encoding: gzip\r\nContent-Encoding: gzip\r\n",
        "Content-Encoding: \r\n",
    ] {
        assert_eq!(
            fetch_response(response(200, headers, b"abc"), bounded())
                .await
                .unwrap_err(),
            DownloadError::UnsupportedEncoding
        );
    }
}

#[tokio::test]
async fn relative_redirects_are_validated_and_reported() {
    let fixture = Fixture::serve(vec![
        response(302, "Location: /final\r\n", b""),
        response(200, "", b"abc"),
    ])
    .await;
    let result = client()
        .fetch(
            fixture.request(bounded(), unhashed()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.evidence().redirects, 1);
    let requests = fixture.finish().await;
    assert!(requests[1].starts_with(b"GET /final HTTP/1.1\r\n"));
    assert!(
        !String::from_utf8_lossy(&requests[1])
            .to_ascii_lowercase()
            .contains("referer:")
    );
}

#[tokio::test]
async fn explicitly_allowed_cross_origin_get_redirects_succeed() {
    let destination = Fixture::serve(vec![response(200, "", b"abc")]).await;
    let source = Fixture::serve(vec![response(
        307,
        &format!("Location: {}/final\r\n", destination.origin),
        b"",
    )])
    .await;
    let request = DownloadRequest::get(
        &source.origin,
        OriginPolicy::loopback_for_tests([&source.origin, &destination.origin], 1).unwrap(),
        bounded(),
        unhashed(),
    );
    let result = client()
        .fetch(request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.evidence().final_origin, destination.origin);
    source.finish().await;
    destination.finish().await;
}

#[tokio::test]
async fn redirect_origin_and_method_rejections_prevent_destination_requests() {
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}", destination.local_addr().unwrap());
    for post in [false, true] {
        let source = Fixture::serve(vec![response(
            307,
            &format!("Location: {target}/target\r\n"),
            b"",
        )])
        .await;
        let origins = if post {
            OriginPolicy::loopback_for_tests([&source.origin, &target], 1).unwrap()
        } else {
            OriginPolicy::loopback_for_tests([&source.origin], 1).unwrap()
        };
        let request = if post {
            DownloadRequest::post_json(
                &source.origin,
                serde_json::json!({"query": "private"}),
                origins,
                bounded(),
                unhashed(),
            )
        } else {
            DownloadRequest::get(&source.origin, origins, bounded(), unhashed())
        };
        let expected = if post {
            DownloadError::RedirectMethod
        } else {
            DownloadError::OriginNotAllowed
        };
        assert_eq!(
            client()
                .fetch(request, &CancellationToken::new())
                .await
                .unwrap_err(),
            expected
        );
        source.finish().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), destination.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn invalid_and_insecure_redirects_are_rejected() {
    for (headers, expected) in [
        ("", DownloadError::InvalidRedirect),
        ("Location: \r\n", DownloadError::InvalidRedirect),
        (
            "Location: /first\r\nLocation: /second\r\n",
            DownloadError::InvalidRedirect,
        ),
        (
            "Location: http://example.com/artifact\r\n",
            DownloadError::InsecureOrigin,
        ),
        (
            "Location: https://user:secret@example.com/artifact\r\n",
            DownloadError::InvalidUrl,
        ),
        (
            "Location: /artifact#fragment\r\n",
            DownloadError::InvalidUrl,
        ),
    ] {
        assert_eq!(
            fetch_response(response(302, headers, b""), bounded())
                .await
                .unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn redirect_loop_stops_at_the_configured_bound() {
    let fixture = Fixture::serve(vec![response(302, "Location: /again\r\n", b""); 3]).await;
    let request = DownloadRequest::get(
        &fixture.origin,
        OriginPolicy::loopback_for_tests([&fixture.origin], 2).unwrap(),
        bounded(),
        unhashed(),
    );
    assert_eq!(
        client()
            .fetch(request, &CancellationToken::new())
            .await
            .unwrap_err(),
        DownloadError::RedirectLimit
    );
    assert_eq!(fixture.finish().await.len(), 3);
}

#[tokio::test]
async fn post_redirects_preserve_body_only_for_same_origin_307_and_308() {
    for status in [307, 308] {
        let fixture = Fixture::serve(vec![
            response(status, "Location: /final\r\n", b""),
            response(200, "", b"abc"),
        ])
        .await;
        let request = DownloadRequest::post_json(
            &fixture.origin,
            serde_json::json!({"ids": ["abc"]}),
            OriginPolicy::loopback_for_tests([&fixture.origin], 1).unwrap(),
            bounded(),
            unhashed(),
        );
        client()
            .fetch(request, &CancellationToken::new())
            .await
            .unwrap();
        let requests = fixture.finish().await;
        assert!(requests[1].starts_with(b"POST /final HTTP/1.1\r\n"));
        for request in requests {
            assert!(request.ends_with(br#"{"ids":["abc"]}"#));
            assert!(String::from_utf8_lossy(&request).contains("content-type: application/json"));
        }
    }
    for status in [301, 302, 303] {
        let fixture = Fixture::serve(vec![response(status, "Location: /final\r\n", b"")]).await;
        let request = DownloadRequest::post_json(
            &fixture.origin,
            serde_json::json!({"ids": []}),
            OriginPolicy::loopback_for_tests([&fixture.origin], 1).unwrap(),
            bounded(),
            unhashed(),
        );
        assert_eq!(
            client()
                .fetch(request, &CancellationToken::new())
                .await
                .unwrap_err(),
            DownloadError::RedirectMethod
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn oversized_query_is_rejected_before_any_network_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let request = DownloadRequest::post_json(
        &origin,
        serde_json::json!({"query": "a".repeat(1024 * 1024)}),
        OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
        bounded(),
        unhashed(),
    );
    assert_eq!(
        client()
            .fetch(request, &CancellationToken::new())
            .await
            .unwrap_err(),
        DownloadError::RequestBodyTooLarge
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pre_cancelled_fetch_makes_no_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let request = DownloadRequest::get(
        &origin,
        OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
        bounded(),
        unhashed(),
    );
    let token = CancellationToken::new();
    token.cancel();
    assert_eq!(
        client().fetch(request, &token).await.unwrap_err(),
        DownloadError::Cancelled
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

// Retain the server socket until the test is done, so EOF cannot accidentally
// satisfy a cancellation or timeout assertion intended for stalled I/O.
async fn stalled_fetch(prefix: &'static [u8], config: ClientConfig, cancel: bool) -> DownloadError {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let request = DownloadRequest::get(
        &origin,
        OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
        bounded(),
        unhashed(),
    );
    let token = CancellationToken::new();
    let fetch_token = token.clone();
    let fetch = tokio::spawn(async move {
        ProviderClient::new(config)
            .unwrap()
            .fetch(request, &fetch_token)
            .await
    });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    read_request(&mut stream).await;
    stream.write_all(prefix).await.unwrap();
    if cancel {
        token.cancel();
    }
    let error = tokio::time::timeout(Duration::from_secs(2), fetch)
        .await
        .expect("fetch did not settle")
        .unwrap()
        .unwrap_err();
    drop(stream);
    error
}

#[tokio::test]
async fn cancellation_interrupts_headers_and_incomplete_body_without_partial_success() {
    for prefix in [
        b"".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\na".as_slice(),
    ] {
        assert_eq!(
            stalled_fetch(prefix, ClientConfig::default(), true).await,
            DownloadError::Cancelled
        );
    }
}

#[tokio::test]
async fn read_and_whole_operation_timeouts_interrupt_stalled_io() {
    for config in [
        ClientConfig {
            read_timeout: Duration::from_millis(50),
            total_timeout: Duration::from_secs(1),
            ..ClientConfig::default()
        },
        ClientConfig {
            read_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_millis(50),
            ..ClientConfig::default()
        },
    ] {
        assert_eq!(
            stalled_fetch(
                b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\na",
                config,
                false
            )
            .await,
            DownloadError::Timeout
        );
    }
}

#[test]
fn retry_advice_distinguishes_transient_failures_from_policy_and_integrity() {
    for error in [
        DownloadError::Timeout,
        DownloadError::Network,
        DownloadError::IncompleteBody,
        DownloadError::HttpStatus { status: 408 },
        DownloadError::HttpStatus { status: 429 },
        DownloadError::HttpStatus { status: 503 },
    ] {
        assert!(error.is_retryable());
    }
    for error in [
        DownloadError::Cancelled,
        DownloadError::ChecksumMismatch,
        DownloadError::SizeMismatch,
        DownloadError::InvalidEncoding,
        DownloadError::OriginNotAllowed,
        DownloadError::HttpStatus { status: 404 },
        DownloadError::DecodedLimitExceeded { limit: 10 },
    ] {
        assert!(!error.is_retryable());
    }
}
