use super::types::{LoaderError, LoaderProviderFailureKind};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use std::sync::OnceLock;
use std::time::Duration;

const USER_AGENT: &str = "axial/0.3";
const MAX_LOADER_JSON_BYTES: usize = 8 * 1024 * 1024;
const LOADER_HTTP_CLIENT_MAX_IDLE_PER_HOST: usize = 8;
const LOADER_HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 120;
const LOADER_HTTP_CLIENT_TCP_KEEPALIVE_SECS: u64 = 60;
const LOADER_SOURCE_REDIRECT_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoaderSourceRedirectDecision {
    Follow,
    RejectInsecure,
    RejectLimit,
}

#[derive(Clone, Copy)]
enum LoaderSourceTransportPolicy {
    HttpsOnly,
    #[cfg(test)]
    AllowHttpForTest,
}

impl LoaderSourceTransportPolicy {
    fn requires_https(self) -> bool {
        match self {
            Self::HttpsOnly => true,
            #[cfg(test)]
            Self::AllowHttpForTest => false,
        }
    }
}

pub async fn fetch_json<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned + Send + 'static,
{
    fetch_json_with_policy(url, LoaderSourceTransportPolicy::HttpsOnly).await
}

#[cfg(test)]
pub(crate) async fn fetch_json_for_test<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned + Send + 'static,
{
    fetch_json_with_policy(url, LoaderSourceTransportPolicy::AllowHttpForTest).await
}

async fn fetch_json_with_policy<T>(
    url: &str,
    policy: LoaderSourceTransportPolicy,
) -> Result<T, LoaderError>
where
    T: DeserializeOwned + Send + 'static,
{
    if policy.requires_https() && !loader_source_url_is_secure(url) {
        return Err(insecure_loader_source_error(
            "loader provider source must use HTTPS",
        ));
    }
    let mut last_error: Option<LoaderError> = None;
    for attempt in 0..3 {
        match fetch_json_once::<T>(url, policy).await {
            Ok(value) => return Ok(value),
            Err(error) if !loader_error_allows_primitive_retry(&error) => return Err(error),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    retry_delay(attempt).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| provider_network_error(None)))
}

pub async fn fetch_bytes(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    fetch_bytes_with_policy(url, max_size, LoaderSourceTransportPolicy::HttpsOnly).await
}

#[cfg(test)]
pub(crate) async fn fetch_bytes_for_test(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    fetch_bytes_with_policy(url, max_size, LoaderSourceTransportPolicy::AllowHttpForTest).await
}

async fn fetch_bytes_with_policy(
    url: &str,
    max_size: u64,
    policy: LoaderSourceTransportPolicy,
) -> Result<Vec<u8>, LoaderError> {
    if policy.requires_https() && !loader_source_url_is_secure(url) {
        return Err(insecure_loader_source_error("loader source must use HTTPS"));
    }
    let mut last_error: Option<LoaderError> = None;
    for attempt in 0..3 {
        match fetch_bytes_once(url, max_size, policy).await {
            Ok(value) => return Ok(value),
            Err(LoaderError::ArtifactMissing(message)) => {
                return Err(LoaderError::ArtifactMissing(message));
            }
            Err(error) if !loader_error_allows_primitive_retry(&error) => return Err(error),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    retry_delay(attempt).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| provider_network_error(None)))
}

async fn fetch_json_once<T>(
    url: &str,
    policy: LoaderSourceTransportPolicy,
) -> Result<T, LoaderError>
where
    T: DeserializeOwned,
{
    let response = source_client(policy)
        .get(url)
        .send()
        .await
        .map_err(|error| provider_network_error(Some(error)))?;
    if policy.requires_https() && response.url().scheme() != "https" {
        return Err(insecure_loader_source_error(
            "loader provider source redirected to an insecure URL",
        ));
    }
    if !response.status().is_success() {
        return Err(provider_status_error(response.status()));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_LOADER_JSON_BYTES as u64)
    {
        return Err(loader_response_too_large());
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| provider_network_error(Some(error)))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_LOADER_JSON_BYTES {
            return Err(loader_response_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| LoaderError::ProviderDataInvalid {
        kind: LoaderProviderFailureKind::SchemaInvalid,
        status: None,
    })
}

fn loader_response_too_large() -> LoaderError {
    LoaderError::ProviderDataInvalid {
        kind: LoaderProviderFailureKind::ResponseTooLarge,
        status: None,
    }
}

async fn fetch_bytes_once(
    url: &str,
    max_size: u64,
    policy: LoaderSourceTransportPolicy,
) -> Result<Vec<u8>, LoaderError> {
    let response = source_client(policy)
        .get(url)
        .send()
        .await
        .map_err(|error| provider_network_error(Some(error)))?;
    if policy.requires_https() && response.url().scheme() != "https" {
        return Err(insecure_loader_source_error(
            "loader source redirected to an insecure URL",
        ));
    }

    let mut bytes = Vec::new();
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(LoaderError::ArtifactMissing(
            "artifact returned HTTP 404".to_string(),
        ));
    }
    if !status.is_success() {
        return Err(provider_status_error(status));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > max_size)
    {
        return Err(loader_response_too_large());
    }

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| provider_network_error(Some(error)))?;
        if bytes.len() as u64 + chunk.len() as u64 > max_size {
            return Err(loader_response_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn loader_source_url_is_secure(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn loader_source_redirect_decision(
    destination: &reqwest::Url,
    previous_count: usize,
) -> LoaderSourceRedirectDecision {
    if previous_count >= LOADER_SOURCE_REDIRECT_LIMIT {
        LoaderSourceRedirectDecision::RejectLimit
    } else if destination.scheme() == "https" {
        LoaderSourceRedirectDecision::Follow
    } else {
        LoaderSourceRedirectDecision::RejectInsecure
    }
}

fn insecure_loader_source_error(message: &str) -> LoaderError {
    LoaderError::InstallExecutionFailed(message.to_string())
}

fn provider_network_error(error: Option<reqwest::Error>) -> LoaderError {
    let kind = if error.as_ref().is_some_and(reqwest::Error::is_timeout) {
        LoaderProviderFailureKind::Timeout
    } else {
        LoaderProviderFailureKind::Network
    };
    LoaderError::ProviderUnavailable { kind, status: None }
}

fn provider_status_error(status: reqwest::StatusCode) -> LoaderError {
    let kind = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        LoaderProviderFailureKind::HttpRateLimited
    } else if status.is_server_error() {
        LoaderProviderFailureKind::HttpServer
    } else if status == reqwest::StatusCode::NOT_FOUND {
        LoaderProviderFailureKind::HttpNotFound
    } else {
        LoaderProviderFailureKind::HttpStatus
    };
    LoaderError::ProviderUnavailable {
        kind,
        status: Some(status.as_u16()),
    }
}

async fn retry_delay(attempt: usize) {
    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
}

fn loader_error_allows_primitive_retry(error: &LoaderError) -> bool {
    matches!(
        error,
        LoaderError::ProviderUnavailable {
            kind: LoaderProviderFailureKind::Network
                | LoaderProviderFailureKind::Timeout
                | LoaderProviderFailureKind::HttpServer,
            ..
        }
    )
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(LOADER_HTTP_CLIENT_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(Duration::from_secs(
                LOADER_HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
            ))
            .tcp_keepalive(Duration::from_secs(LOADER_HTTP_CLIENT_TCP_KEEPALIVE_SECS))
            .build()
            .expect("loader HTTP client configuration should be valid")
    })
}

fn source_client(policy: LoaderSourceTransportPolicy) -> &'static reqwest::Client {
    if !policy.requires_https() {
        return client();
    }
    static SOURCE_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    SOURCE_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                match loader_source_redirect_decision(attempt.url(), attempt.previous().len()) {
                    LoaderSourceRedirectDecision::Follow => attempt.follow(),
                    LoaderSourceRedirectDecision::RejectInsecure => {
                        attempt.error("loader source redirect must use HTTPS")
                    }
                    LoaderSourceRedirectDecision::RejectLimit => {
                        attempt.error("loader source redirect limit exceeded")
                    }
                }
            }))
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(LOADER_HTTP_CLIENT_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(Duration::from_secs(
                LOADER_HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
            ))
            .tcp_keepalive(Duration::from_secs(LOADER_HTTP_CLIENT_TCP_KEEPALIVE_SECS))
            .build()
            .expect("loader source HTTP client configuration should be valid")
    })
}

#[cfg(test)]
mod tests {
    use super::{
        LOADER_SOURCE_REDIRECT_LIMIT, LoaderSourceRedirectDecision, LoaderSourceTransportPolicy,
        MAX_LOADER_JSON_BYTES, fetch_bytes, fetch_bytes_for_test, fetch_json, fetch_json_for_test,
        fetch_json_once, loader_source_redirect_decision,
    };
    use crate::loaders::types::{LoaderError, LoaderProviderFailureKind};
    use serde::Deserialize;
    use serde_json::Value;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn production_redirect_policy_rejects_https_to_http_downgrade() {
        let destination = reqwest::Url::parse("http://provider.example.test/artifact")
            .expect("valid redirect destination");

        assert_eq!(
            loader_source_redirect_decision(&destination, 0),
            LoaderSourceRedirectDecision::RejectInsecure
        );
    }

    #[test]
    fn production_redirect_policy_enforces_redirect_limit() {
        let destination = reqwest::Url::parse("https://provider.example.test/artifact")
            .expect("valid redirect destination");

        assert_eq!(
            loader_source_redirect_decision(&destination, LOADER_SOURCE_REDIRECT_LIMIT - 1),
            LoaderSourceRedirectDecision::Follow
        );
        assert_eq!(
            loader_source_redirect_decision(&destination, LOADER_SOURCE_REDIRECT_LIMIT),
            LoaderSourceRedirectDecision::RejectLimit
        );
    }

    #[tokio::test]
    async fn fetch_bytes_maps_http_404_to_artifact_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set test server nonblocking");
        let url = format!(
            "http://{}/missing-installer.jar",
            listener.local_addr().expect("server addr")
        );
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_request_count = Arc::clone(&request_count);
        let (stop_server, server_stopped) = mpsc::channel();
        let server = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        server_request_count.fetch_add(1, Ordering::SeqCst);
                        respond_404(stream);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        if server_stopped.try_recv().is_ok() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept connection: {error}"),
                }
            }
        });

        let error = fetch_bytes_for_test(&url, 1024)
            .await
            .expect_err("404 error");

        match error {
            LoaderError::ArtifactMissing(message) => {
                assert!(message.contains("HTTP 404"), "{message}");
                assert!(!message.contains(&url), "{message}");
            }
            error => panic!("expected ArtifactMissing, got {error:?}"),
        }

        stop_server.send(()).expect("stop test server");
        server.join().expect("server thread");
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fetch_bytes_rejects_response_larger_than_max_size() {
        let server = TestServer::spawn(|stream| {
            respond(
                stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde",
            );
        });

        let error = fetch_bytes_for_test(&server.url("/installer.jar"), 4)
            .await
            .expect_err("oversized response");

        match error {
            LoaderError::ProviderDataInvalid { kind, status } => {
                assert_eq!(kind, LoaderProviderFailureKind::ResponseTooLarge);
                assert_eq!(status, None);
            }
            error => panic!("expected ProviderDataInvalid, got {error:?}"),
        }
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn production_loader_source_rejects_http_before_request() {
        let server = TestServer::spawn(|stream| {
            respond(
                stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        });

        let error = fetch_bytes(&server.url("/installer.jar"), 16)
            .await
            .expect_err("production loader source must reject HTTP");

        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader source must use HTTPS"
        ));
        assert_eq!(server.request_count(), 0);
    }

    #[derive(Debug, Deserialize)]
    struct TestPayload {
        value: String,
    }

    #[tokio::test]
    async fn fetch_json_parses_successful_response() {
        let server = TestServer::spawn(|stream| {
            respond(
                stream,
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"value\":\"ok\"}",
            );
        });

        let payload: TestPayload = fetch_json_for_test(&server.url("/metadata.json"))
            .await
            .expect("json response");

        assert_eq!(payload.value, "ok");
    }

    #[tokio::test]
    async fn fetch_json_rejects_oversized_content_length() {
        let server = TestServer::spawn(|stream| {
            respond_oversized_json_content_length(stream);
        });

        let error = fetch_json_once::<Value>(
            &server.url("/metadata.json"),
            LoaderSourceTransportPolicy::AllowHttpForTest,
        )
        .await
        .expect_err("oversized response");

        assert_loader_json_too_large(error);
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn fetch_json_rejects_chunked_response_past_max_size() {
        let server = TestServer::spawn(|stream| {
            respond_oversized_chunked_json(stream);
        });

        let error = fetch_json_once::<Value>(
            &server.url("/metadata.json"),
            LoaderSourceTransportPolicy::AllowHttpForTest,
        )
        .await
        .expect_err("oversized response");

        assert_loader_json_too_large(error);
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn fetch_json_classifies_http_status_without_url_or_body() {
        let server = TestServer::spawn(|stream| {
            respond(
                stream,
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 33\r\nConnection: close\r\n\r\n{\"token\":\"secret\",\"path\":\"/tmp/x\"}",
            );
        });

        let error = fetch_json_once::<Value>(
            &server.url("/metadata.json"),
            LoaderSourceTransportPolicy::AllowHttpForTest,
        )
        .await
        .expect_err("http status should fail");

        match error {
            LoaderError::ProviderUnavailable { kind, status } => {
                assert_eq!(kind, LoaderProviderFailureKind::HttpRateLimited);
                assert_eq!(status, Some(429));
            }
            error => panic!("expected ProviderUnavailable, got {error:?}"),
        }
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn fetch_json_classifies_schema_drift_without_payload() {
        let server = TestServer::spawn(|stream| {
            respond(
                stream,
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\nConnection: close\r\n\r\n{not-json",
            );
        });

        let error = fetch_json_once::<Value>(
            &server.url("/metadata.json"),
            LoaderSourceTransportPolicy::AllowHttpForTest,
        )
        .await
        .expect_err("invalid json should fail");
        let encoded = error.to_string().to_ascii_lowercase();

        match &error {
            LoaderError::ProviderDataInvalid { kind, status } => {
                assert_eq!(*kind, LoaderProviderFailureKind::SchemaInvalid);
                assert_eq!(*status, None);
            }
            error => panic!("expected ProviderDataInvalid, got {error:?}"),
        }
        assert!(!encoded.contains("not-json"));
        assert!(!encoded.contains("metadata.json"));
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn production_loader_provider_rejects_http_before_request() {
        let server = TestServer::spawn(|stream| {
            respond(
                stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
        });

        let error = fetch_json::<Value>(&server.url("/metadata.json"))
            .await
            .expect_err("production loader provider must reject HTTP");

        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader provider source must use HTTPS"
        ));
        assert_eq!(server.request_count(), 0);
    }

    fn respond_404(stream: TcpStream) {
        respond(
            stream,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
        );
    }

    fn respond_oversized_json_content_length(stream: TcpStream) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_LOADER_JSON_BYTES + 1
        );
        respond(stream, response.as_bytes());
    }

    fn respond_oversized_chunked_json(mut stream: TcpStream) {
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            MAX_LOADER_JSON_BYTES + 1
        )
        .expect("write response headers");
        stream
            .write_all(&vec![b' '; MAX_LOADER_JSON_BYTES + 1])
            .expect("write response body");
        stream.write_all(b"\r\n0\r\n\r\n").expect("finish response");
    }

    fn respond(mut stream: TcpStream, response: &[u8]) {
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer);
        stream.write_all(response).expect("write response");
    }

    fn assert_loader_json_too_large(error: LoaderError) {
        match error {
            LoaderError::ProviderDataInvalid { kind, status } => {
                assert_eq!(kind, LoaderProviderFailureKind::ResponseTooLarge);
                assert_eq!(status, None);
            }
            error => panic!("expected ProviderDataInvalid, got {error:?}"),
        }
    }

    struct TestServer {
        address: std::net::SocketAddr,
        request_count: Arc<AtomicUsize>,
        stop_server: mpsc::Sender<()>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(respond: fn(TcpStream)) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set test server nonblocking");
            let address = listener.local_addr().expect("server addr");
            let request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&request_count);
            let (stop_server, server_stopped) = mpsc::channel();
            let server = thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            server_request_count.fetch_add(1, Ordering::SeqCst);
                            respond(stream);
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            if server_stopped.try_recv().is_ok() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept connection: {error}"),
                    }
                }
            });

            Self {
                address,
                request_count,
                stop_server,
                server: Some(server),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.address, path)
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.stop_server.send(());
            if let Some(server) = self.server.take() {
                server.join().expect("server thread");
            }
        }
    }
}
