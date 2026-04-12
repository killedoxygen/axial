use super::types::LoaderError;
use serde::de::DeserializeOwned;
use std::io::Read;
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
        .map_err(|error| LoaderError::Other(format!("request failed for {url}: {error}")))?;

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

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .timeout_write(Duration::from_secs(120))
        .user_agent(USER_AGENT)
        .build()
}
