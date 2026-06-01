use super::types::LoaderError;
use serde::de::DeserializeOwned;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

const USER_AGENT: &str = "croopor/0.3";

pub async fn fetch_json<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned + Send + 'static,
{
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let mut last_error: Option<LoaderError> = None;
        for attempt in 0..3 {
            match fetch_json_blocking::<T>(&url) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(250 * (attempt + 1) as u64));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| LoaderError::Other(format!("request failed for {url}"))))
    })
    .await
    .map_err(|error| LoaderError::Other(format!("loader fetch task failed: {error}")))?
}

pub async fn fetch_bytes(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let mut last_error: Option<LoaderError> = None;
        for attempt in 0..3 {
            match fetch_bytes_blocking(&url, max_size) {
                Ok(value) => return Ok(value),
                Err(LoaderError::ArtifactMissing(message)) => {
                    return Err(LoaderError::ArtifactMissing(message));
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(250 * (attempt + 1) as u64));
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| LoaderError::Other(format!("request failed for {url}"))))
    })
    .await
    .map_err(|error| LoaderError::Other(format!("loader fetch task failed: {error}")))?
}

fn fetch_json_blocking<T>(url: &str) -> Result<T, LoaderError>
where
    T: DeserializeOwned,
{
    let response = agent()
        .get(url)
        .call()
        .map_err(|error| LoaderError::Other(format!("request failed for {url}: {error}")))?;
    serde_json::from_reader(response.into_reader()).map_err(LoaderError::Parse)
}

fn fetch_bytes_blocking(url: &str, max_size: u64) -> Result<Vec<u8>, LoaderError> {
    let response = agent()
        .get(url)
        .call()
        .map_err(|error| map_bytes_request_error(url, error))?;

    let mut reader = response.into_reader();
    let mut limited = (&mut reader).take(max_size + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_size {
        return Err(LoaderError::ArtifactMissing(format!(
            "download too large for {url}"
        )));
    }
    Ok(bytes)
}

fn map_bytes_request_error(url: &str, error: ureq::Error) -> LoaderError {
    match error {
        ureq::Error::Status(404, _) => {
            LoaderError::ArtifactMissing(format!("artifact returned HTTP 404 for {url}"))
        }
        error => LoaderError::Other(format!("request failed for {url}: {error}")),
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(120))
            .timeout_write(Duration::from_secs(120))
            .user_agent(USER_AGENT)
            .build()
    })
}

#[cfg(test)]
mod tests {
    use super::fetch_bytes;
    use crate::loaders::types::LoaderError;
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

    fn respond_404(mut stream: TcpStream) {
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer);
        stream
            .write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
            )
            .expect("write response");
    }
}
