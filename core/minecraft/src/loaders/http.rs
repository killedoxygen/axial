use super::types::LoaderError;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use std::sync::OnceLock;
use std::time::Duration;

const USER_AGENT: &str = "croopor/0.3";

pub async fn fetch_json<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned + Send + 'static,
{
    let mut last_error: Option<LoaderError> = None;
    for attempt in 0..3 {
        match fetch_json_once::<T>(url).await {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    retry_delay(attempt).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| LoaderError::Other(format!("request failed for {url}"))))
}

pub async fn fetch_bytes(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    let mut last_error: Option<LoaderError> = None;
    for attempt in 0..3 {
        match fetch_bytes_once(url, max_size).await {
            Ok(value) => return Ok(value),
            Err(LoaderError::ArtifactMissing(message)) => {
                return Err(LoaderError::ArtifactMissing(message));
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    retry_delay(attempt).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| LoaderError::Other(format!("request failed for {url}"))))
}

async fn fetch_json_once<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned,
{
    let response = client()
        .get(url)
        .send()
        .await
        .map_err(|error| LoaderError::Other(format!("request failed for {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(LoaderError::Other(format!(
            "request failed for {url}: HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| LoaderError::Other(format!("request failed for {url}: {error}")))?;
    serde_json::from_slice(&bytes).map_err(LoaderError::Parse)
}

async fn fetch_bytes_once(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    let response = client()
        .get(url)
        .send()
        .await
        .map_err(|error| LoaderError::Other(format!("request failed for {url}: {error}")))?;

    let mut bytes = Vec::new();
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(LoaderError::ArtifactMissing(format!(
            "artifact returned HTTP 404 for {url}"
        )));
    }
    if !status.is_success() {
        return Err(LoaderError::Other(format!(
            "request failed for {url}: HTTP {status}"
        )));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > max_size)
    {
        return Err(LoaderError::ArtifactMissing(format!(
            "download too large for {url}"
        )));
    }

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| LoaderError::Other(format!("request failed for {url}: {error}")))?;
        if bytes.len() as u64 + chunk.len() as u64 > max_size {
            return Err(LoaderError::ArtifactMissing(format!(
                "download too large for {url}"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn retry_delay(attempt: usize) {
    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[cfg(test)]
mod tests {
    use super::{fetch_bytes, fetch_json};
    use crate::loaders::types::LoaderError;
    use serde::Deserialize;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

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

        let error = fetch_bytes(&url, 1024).await.expect_err("404 error");

        match error {
            LoaderError::ArtifactMissing(message) => {
                assert!(message.contains("HTTP 404"), "{message}");
                assert!(message.contains(&url), "{message}");
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

        let error = fetch_bytes(&server.url("/installer.jar"), 4)
            .await
            .expect_err("oversized response");

        match error {
            LoaderError::ArtifactMissing(message) => {
                assert!(message.contains("download too large"), "{message}");
            }
            error => panic!("expected ArtifactMissing, got {error:?}"),
        }
        assert_eq!(server.request_count(), 1);
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

        let payload: TestPayload = fetch_json(&server.url("/metadata.json"))
            .await
            .expect("json response");

        assert_eq!(payload.value, "ok");
    }

    fn respond_404(stream: TcpStream) {
        respond(
            stream,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
        );
    }

    fn respond(mut stream: TcpStream, response: &[u8]) {
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer);
        stream.write_all(response).expect("write response");
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
