//! Shared HTTP policy: client construction, transient-failure classification and
//! bounded, digest-checked fetches. Dropping a fetch future cancels it; no partial
//! body escapes.

use reqwest::header::{CONTENT_LENGTH, HeaderMap, TRANSFER_ENCODING};
use sha1::{Digest as _, Sha1};
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

pub const USER_AGENT: &str = concat!("axial/", env!("CARGO_PKG_VERSION"));
pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1_500),
    Duration::from_millis(4_000),
];
const MAX_REDIRECTS: usize = 10;

/// Statuses that a later identical request may satisfy.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500..=599)
}

/// Common construction for every Axial client: user agent, timeouts, pooling and
/// HTTPS-only redirects. Callers may narrow the redirect policy further.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(120))
        .tcp_keepalive(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("redirect limit exceeded")
            } else if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.error("redirect must use HTTPS")
            }
        }))
}

/// Requests and redirects use HTTPS only.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        builder()
            .https_only(true)
            .build()
            .expect("HTTPS client configuration is valid")
    })
}

/// Admits legacy plain-HTTP repositories for content bound by a declared digest or
/// signature. Redirects still require HTTPS.
pub fn artifact_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        builder()
            .build()
            .expect("artifact client configuration is valid")
    })
}

/// Literal-loopback HTTP fixtures, fetched without proxies or redirects.
#[cfg(any(test, feature = "test-support"))]
pub fn loopback_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("loopback client configuration is valid")
    })
}

/// HTTPS in production; test builds also admit literal-loopback HTTP fixtures.
pub fn admits(url: &reqwest::Url) -> bool {
    url.scheme() == "https" || (cfg!(any(test, feature = "test-support")) && is_loopback_http(url))
}

/// The client for an admitted source URL without userinfo.
pub fn client_for(url: &str) -> Result<&'static reqwest::Client, FetchError> {
    let url = reqwest::Url::parse(url).map_err(|_| FetchError::Refused)?;
    if !admits(&url)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(FetchError::Refused);
    }
    #[cfg(any(test, feature = "test-support"))]
    if is_loopback_http(&url) {
        return Ok(loopback_client());
    }
    Ok(client())
}

pub fn is_loopback_http(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|address| address.is_loopback())
}

/// Errors omit URLs; `Request` retains the sanitized transport cause and status.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("source URL is not admitted by the transport policy")]
    Refused,
    #[error("request failed")]
    Request(#[source] reqwest::Error),
    #[error("response exceeds its {limit}-byte limit")]
    TooLarge { limit: u64, observed: u64 },
    #[error("response is incomplete")]
    Incomplete,
    #[error("response buffer could not be allocated")]
    Allocation,
    #[error("response size {observed} does not match the expected {expected}")]
    SizeMismatch { expected: u64, observed: u64 },
    #[error("response SHA-1 does not match its metadata")]
    DigestMismatch { observed: [u8; 20] },
}

impl FetchError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Request(error) => error.status().map(|status| status.as_u16()),
            _ => None,
        }
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Request(error) if error.is_timeout())
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Request(error) => match error.status() {
                Some(status) => is_retryable_status(status.as_u16()),
                None => !error.is_redirect() && !error.is_builder(),
            },
            Self::Incomplete => true,
            _ => false,
        }
    }
}

fn request_error(error: reqwest::Error) -> FetchError {
    FetchError::Request(error.without_url())
}

/// A successful response whose body has not yet exceeded `limit`.
pub struct Body {
    response: reqwest::Response,
    declared: Option<u64>,
    limit: u64,
    observed: u64,
}

/// Rejects failure statuses, ambiguous framing and declared lengths past `limit`
/// before any body byte is read.
pub fn admit(response: reqwest::Response, limit: u64) -> Result<Body, FetchError> {
    let response = response.error_for_status().map_err(request_error)?;
    let headers = response.headers();
    let declared = response.content_length();
    if headers.get_all(CONTENT_LENGTH).iter().nth(1).is_some()
        || (headers.contains_key(CONTENT_LENGTH) && headers.contains_key(TRANSFER_ENCODING))
    {
        return Err(FetchError::Incomplete);
    }
    if let Some(declared) = declared.filter(|declared| *declared > limit) {
        return Err(FetchError::TooLarge {
            limit,
            observed: declared,
        });
    }
    Ok(Body {
        response,
        declared,
        limit,
        observed: 0,
    })
}

impl Body {
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    pub fn declared_length(&self) -> Option<u64> {
        self.declared
    }

    pub async fn chunk(&mut self) -> Result<Option<impl AsRef<[u8]>>, FetchError> {
        match self.response.chunk().await.map_err(request_error)? {
            Some(chunk) => {
                let observed = self.observed.saturating_add(chunk.len() as u64);
                if observed > self.limit {
                    return Err(FetchError::TooLarge {
                        limit: self.limit,
                        observed,
                    });
                }
                self.observed = observed;
                Ok(Some(chunk))
            }
            None if self
                .declared
                .is_some_and(|declared| declared != self.observed) =>
            {
                Err(FetchError::Incomplete)
            }
            None => Ok(None),
        }
    }

    pub async fn bytes(mut self) -> Result<Vec<u8>, FetchError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            let chunk = chunk.as_ref();
            bytes
                .try_reserve(chunk.len())
                .map_err(|_| FetchError::Allocation)?;
            bytes.extend_from_slice(chunk);
        }
        Ok(bytes)
    }
}

/// Sends `request` and admits its response within `limit`.
pub async fn send(request: reqwest::RequestBuilder, limit: u64) -> Result<Body, FetchError> {
    admit(request.send().await.map_err(request_error)?, limit)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Expected {
    pub size: Option<u64>,
    pub sha1: Option<[u8; 20]>,
}

#[derive(Debug)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub sha1: [u8; 20],
}

/// One GET within `limit`, verified against `expected` before the bytes escape.
pub async fn fetch_once(
    client: &reqwest::Client,
    url: &str,
    limit: u64,
    expected: Expected,
) -> Result<Fetched, FetchError> {
    if let Some(size) = expected.size.filter(|size| *size > limit) {
        return Err(FetchError::TooLarge {
            limit,
            observed: size,
        });
    }
    let bytes = send(client.get(url), limit).await?.bytes().await?;
    let observed = bytes.len() as u64;
    if let Some(expected) = expected.size.filter(|size| *size != observed) {
        return Err(FetchError::SizeMismatch { expected, observed });
    }
    let sha1: [u8; 20] = Sha1::digest(&bytes).into();
    if expected.sha1.is_some_and(|expected| expected != sha1) {
        return Err(FetchError::DigestMismatch { observed: sha1 });
    }
    Ok(Fetched { bytes, sha1 })
}

/// `fetch_once`, repeated after transient failures with the shared backoff.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    limit: u64,
    expected: Expected,
) -> Result<Fetched, FetchError> {
    retry(
        &RETRY_DELAYS,
        || fetch_once(client, url, limit, expected),
        FetchError::is_retryable,
    )
    .await
}

/// Repeats `attempt` after each retryable failure until `delays` are exhausted.
pub async fn retry<T, E, F, Fut>(
    delays: &[Duration],
    mut attempt: F,
    retryable: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut delays = delays.iter();
    loop {
        match attempt().await {
            Err(error) if retryable(&error) => match delays.next() {
                Some(delay) => tokio::time::sleep(*delay).await,
                None => return Err(error),
            },
            result => return result,
        }
    }
}

/// Loopback HTTP servers for tests: each records request heads before responding.
#[cfg(test)]
pub(crate) mod test_server {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) struct Server {
        pub(crate) origin: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    pub(crate) fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
        if !headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
        }) {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    /// Serves one scripted raw response per connection, in order, then stops accepting.
    pub(crate) async fn serve(responses: Vec<Vec<u8>>) -> Server {
        start(move |listener, requests| async move {
            for response in responses {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                exchange(stream, &requests, |_| response).await;
            }
        })
    }

    /// Serves every connection until finished, answering with `route(request_path)`.
    /// Requires a Tokio runtime context; binding is synchronous.
    pub(crate) fn serve_routes(route: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Server {
        let route = Arc::new(route);
        start(move |listener, requests| async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (route, requests) = (Arc::clone(&route), Arc::clone(&requests));
                tokio::spawn(async move { exchange(stream, &requests, |path| route(path)).await });
            }
        })
    }

    fn start<F: std::future::Future<Output = ()> + Send + 'static>(
        accept: impl FnOnce(tokio::net::TcpListener, Arc<Mutex<Vec<String>>>) -> F,
    ) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(accept(listener, Arc::clone(&requests)));
        Server {
            origin,
            requests,
            task,
        }
    }

    async fn exchange(
        mut stream: tokio::net::TcpStream,
        requests: &Mutex<Vec<String>>,
        respond: impl FnOnce(&str) -> Vec<u8>,
    ) {
        let mut head = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !head.windows(4).any(|end| end == b"\r\n\r\n") {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let response = respond(request_path(&head));
        requests.lock().unwrap().push(head);
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    }

    fn request_path(head: &str) -> &str {
        head.split_whitespace().nth(1).unwrap_or_default()
    }

    impl Server {
        pub(crate) fn url(&self, path: &str) -> String {
            format!("{}{path}", self.origin)
        }

        /// Request paths received so far.
        pub(crate) fn paths(&self) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .map(|head| request_path(head).to_owned())
                .collect()
        }

        pub(crate) fn finish(self) -> Vec<String> {
            self.task.abort();
            std::mem::take(&mut *self.requests.lock().unwrap())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{response, serve};
    use super::*;

    fn digest(bytes: &[u8]) -> [u8; 20] {
        Sha1::digest(bytes).into()
    }

    #[test]
    fn retryable_statuses_are_transient_only() {
        for status in [408, 425, 429, 500, 503, 599] {
            assert!(is_retryable_status(status), "{status}");
        }
        for status in [200, 301, 400, 401, 403, 404, 409, 600] {
            assert!(!is_retryable_status(status), "{status}");
        }
        assert!(FetchError::Incomplete.is_retryable());
        for error in [
            FetchError::Refused,
            FetchError::TooLarge {
                limit: 1,
                observed: 2,
            },
            FetchError::SizeMismatch {
                expected: 1,
                observed: 2,
            },
            FetchError::DigestMismatch { observed: [0; 20] },
        ] {
            assert!(!error.is_retryable(), "{error:?}");
        }
    }

    #[tokio::test]
    async fn https_policy_refuses_plain_http_before_any_request() {
        let server = serve(vec![response("200 OK", &[], b"ok")]).await;
        for url in [
            "http://192.0.2.1/source",
            "https://user:secret@example.invalid/",
        ] {
            assert!(matches!(client_for(url), Err(FetchError::Refused)), "{url}");
        }
        let error = fetch_once(client(), &server.url("/source"), 16, Expected::default())
            .await
            .unwrap_err();
        assert!(matches!(&error, FetchError::Request(error) if error.is_builder()));
        assert!(!error.is_retryable());
        assert!(server.finish().is_empty());
    }

    #[tokio::test]
    async fn redirects_must_remain_https() {
        let server = serve(vec![response(
            "302 Found",
            &[("Location", "http://192.0.2.1/downgrade")],
            b"",
        )])
        .await;
        let error = fetch_once(
            artifact_client(),
            &server.url("/source"),
            16,
            Expected::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(&error, FetchError::Request(error) if error.is_redirect()));
        assert!(!error.is_retryable());
        assert!(!format!("{error:?}").contains("192.0.2.1"));
        assert_eq!(server.finish().len(), 1);
    }

    #[tokio::test]
    async fn declared_and_streamed_bodies_past_the_limit_are_rejected() {
        let declared = serve(vec![response(
            "200 OK",
            &[("Content-Length", "1048576")],
            b"",
        )])
        .await;
        let error = fetch_once(
            loopback_client(),
            &declared.url("/"),
            4,
            Expected::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FetchError::TooLarge {
                limit: 4,
                observed: 1_048_576
            }
        ));
        declared.finish();

        let chunked = serve(vec![response(
            "200 OK",
            &[("Transfer-Encoding", "chunked")],
            b"5\r\nabcde\r\n0\r\n\r\n",
        )])
        .await;
        let error = fetch_once(loopback_client(), &chunked.url("/"), 4, Expected::default())
            .await
            .unwrap_err();
        assert!(matches!(error, FetchError::TooLarge { limit: 4, .. }));
        chunked.finish();

        let error = fetch_once(
            loopback_client(),
            "http://127.0.0.1:9/",
            4,
            Expected {
                size: Some(5),
                sha1: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FetchError::TooLarge { observed: 5, .. }));
    }

    #[tokio::test]
    async fn size_and_digest_mismatches_never_return_bytes() {
        let server = serve(vec![
            response("200 OK", &[], b"abc"),
            response("200 OK", &[], b"abd"),
            response("200 OK", &[], b"abc"),
        ])
        .await;
        let url = server.url("/artifact");
        let size = fetch_once(
            loopback_client(),
            &url,
            16,
            Expected {
                size: Some(4),
                sha1: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            size,
            FetchError::SizeMismatch {
                expected: 4,
                observed: 3
            }
        ));
        let expected = Expected {
            size: Some(3),
            sha1: Some(digest(b"abc")),
        };
        let mismatch = fetch_once(loopback_client(), &url, 16, expected)
            .await
            .unwrap_err();
        assert!(
            matches!(mismatch, FetchError::DigestMismatch { observed } if observed == digest(b"abd"))
        );
        let fetched = fetch_once(loopback_client(), &url, 16, expected)
            .await
            .unwrap();
        assert_eq!(
            (fetched.bytes.as_slice(), fetched.sha1),
            (b"abc".as_slice(), digest(b"abc"))
        );
        assert_eq!(server.finish().len(), 3);
    }

    #[tokio::test]
    async fn truncated_bodies_are_incomplete_and_retryable() {
        let server = serve(vec![response(
            "200 OK",
            &[("Content-Length", "10")],
            b"short",
        )])
        .await;
        let error = fetch_once(loopback_client(), &server.url("/"), 16, Expected::default())
            .await
            .unwrap_err();
        assert!(error.is_retryable(), "{error:?}");
        server.finish();
    }

    #[tokio::test]
    async fn retry_repeats_transient_statuses_and_stops_on_terminal_ones() {
        let server = serve(vec![
            response("503 Service Unavailable", &[], b""),
            response("425 Too Early", &[], b""),
            response("200 OK", &[], b"ok"),
            response("404 Not Found", &[], b""),
        ])
        .await;
        let url = server.url("/source");
        let attempt = || fetch_once(loopback_client(), &url, 16, Expected::default());
        let delays = [Duration::from_millis(1); 3];
        let fetched = retry(&delays, attempt, FetchError::is_retryable)
            .await
            .unwrap();
        assert_eq!(fetched.bytes, b"ok");
        let error = retry(&delays, attempt, FetchError::is_retryable)
            .await
            .unwrap_err();
        assert_eq!(error.status(), Some(404));
        assert_eq!(server.finish().len(), 4);

        let exhausted = serve(vec![response("429 Too Many Requests", &[], b""); 2]).await;
        let url = exhausted.url("/source");
        let error = retry(
            &delays[..1],
            || fetch_once(loopback_client(), &url, 16, Expected::default()),
            FetchError::is_retryable,
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), Some(429));
        assert_eq!(exhausted.finish().len(), 2);
    }

    #[tokio::test]
    async fn dropping_a_fetch_cancels_it_without_a_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stalled", listener.local_addr().unwrap());
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let fetch = fetch(loopback_client(), &url, 16, Expected::default());
        let accept = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            cancel.send(()).unwrap();
            stream
        });
        tokio::select! {
            _ = fetch => panic!("a stalled fetch cannot complete"),
            _ = cancelled => {}
        }
        drop(accept.await.unwrap());
    }

    #[test]
    fn fetch_futures_stay_small() {
        let size = std::mem::size_of_val(&fetch(
            client(),
            "https://example.invalid/",
            1,
            Expected::default(),
        ));
        assert!(size <= 4 * 1024, "{size}");
    }
}
