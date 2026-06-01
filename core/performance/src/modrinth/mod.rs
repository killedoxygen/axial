use hex::encode;
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

const USER_AGENT: &str = "croopor/0.3.1 (github.com/mateoltd/croopor)";
const RATE_LIMIT_BODY_LIMIT: usize = 4096;

#[derive(Debug, Error)]
pub enum ModrinthError {
    #[error("modrinth request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("modrinth download failed: {0}")]
    Io(#[from] io::Error),
    #[error("modrinth API rate limited; reset after {reset_after_seconds:?} seconds: {body}")]
    RateLimited {
        reset_after_seconds: Option<u64>,
        body: String,
    },
    #[error("modrinth API returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("hash mismatch: expected {expected} got {actual}")]
    HashMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone)]
pub struct ModrinthClient {
    client: Client,
    base_url: String,
}

impl ModrinthClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build modrinth client");

        Self {
            client,
            base_url: "https://api.modrinth.com".to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_base_url(base_url: String) -> Self {
        let mut client = Self::new();
        client.base_url = base_url;
        client
    }

    pub async fn list_versions(
        &self,
        project_id: &str,
        game_versions: &[String],
        loaders: &[String],
    ) -> Result<Vec<Version>, ModrinthError> {
        let mut url = Url::parse(&format!(
            "{}/v2/project/{}/version",
            self.base_url, project_id
        ))
        .expect("valid modrinth url");

        if !game_versions.is_empty() {
            url.query_pairs_mut().append_pair(
                "game_versions",
                &serde_json::to_string(game_versions).expect("serialize game versions"),
            );
        }
        if !loaders.is_empty() {
            url.query_pairs_mut().append_pair(
                "loaders",
                &serde_json::to_string(loaders).expect("serialize loaders"),
            );
        }

        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limited_error(response).await);
            }

            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ModrinthError::Http { status, body });
        }

        let versions = response.json::<Vec<Version>>().await?;
        let mut compatible: Vec<Version> = versions
            .into_iter()
            .filter(|version| matches_any(&version.game_versions, game_versions))
            .filter(|version| matches_any_fold(&version.loaders, loaders))
            .collect();

        compatible.sort_by(|left, right| match (left.featured, right.featured) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => right.date_published.cmp(&left.date_published),
        });

        Ok(compatible)
    }

    pub async fn download_file_to_path(
        &self,
        url: &str,
        expected_sha512: &str,
        temp_path: &Path,
    ) -> Result<(), ModrinthError> {
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limited_error(response).await);
            }

            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ModrinthError::Http { status, body });
        }

        if let Some(parent) = temp_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let result = async {
            let mut output = tokio::fs::File::create(temp_path).await?;
            let mut hasher = Sha512::new();
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                hasher.update(&chunk);
                output.write_all(&chunk).await?;
            }
            output.flush().await?;
            Ok::<String, ModrinthError>(encode(hasher.finalize()))
        }
        .await;

        let actual = match result {
            Ok(actual) => actual,
            Err(error) => {
                let _ = fs::remove_file(temp_path);
                return Err(error);
            }
        };

        if !expected_sha512.trim().is_empty() {
            if !actual.eq_ignore_ascii_case(expected_sha512) {
                let _ = fs::remove_file(temp_path);
                return Err(ModrinthError::HashMismatch {
                    expected: expected_sha512.to_string(),
                    actual,
                });
            }
        }
        Ok(())
    }
}

impl Default for ModrinthClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    pub id: String,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub featured: bool,
    #[serde(default)]
    pub date_published: String,
    #[serde(default)]
    pub files: Vec<VersionFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub hashes: std::collections::HashMap<String, String>,
}

impl Version {
    pub fn primary_file(&self) -> Option<&VersionFile> {
        self.files
            .iter()
            .find(|file| file.primary)
            .or_else(|| self.files.first())
    }
}

fn matches_any(values: &[String], wanted: &[String]) -> bool {
    wanted.is_empty()
        || wanted
            .iter()
            .any(|candidate| values.iter().any(|value| value == candidate))
}

fn matches_any_fold(values: &[String], wanted: &[String]) -> bool {
    wanted.is_empty()
        || wanted.iter().any(|candidate| {
            values
                .iter()
                .any(|value| value.eq_ignore_ascii_case(candidate))
        })
}

async fn rate_limited_error(response: Response) -> ModrinthError {
    let reset_after_seconds = response
        .headers()
        .get("X-Ratelimit-Reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let body = bounded_response_text(response, RATE_LIMIT_BODY_LIMIT).await;
    ModrinthError::RateLimited {
        reset_after_seconds,
        body,
    }
}

async fn bounded_response_text(mut response: Response, limit: usize) -> String {
    let mut body = Vec::new();
    while body.len() < limit {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        let remaining = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    String::from_utf8_lossy(&body).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn list_versions_maps_429_to_rate_limited_error() {
        let base_url = spawn_response_server(
            "429 Too Many Requests",
            vec![("X-Ratelimit-Reset".to_string(), "42".to_string())],
            b"try again later".to_vec(),
        )
        .await;
        let client = ModrinthClient::new_with_base_url(base_url);

        let error = client
            .list_versions("sodium", &["1.21.1".to_string()], &["fabric".to_string()])
            .await
            .expect_err("429 should return typed rate-limit error");

        match error {
            ModrinthError::RateLimited {
                reset_after_seconds,
                body,
            } => {
                assert_eq!(reset_after_seconds, Some(42));
                assert_eq!(body, "try again later");
            }
            other => panic!("expected rate-limit error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_file_to_path_streams_and_verifies_sha512() {
        let body = b"managed-jar".to_vec();
        let sha512 = encode(Sha512::digest(&body));
        let url = spawn_file_server(body.clone()).await;
        let root = test_root("download-stream-success");
        let temp_path = root.join("sodium.jar.tmp");
        let client = ModrinthClient::new();

        client
            .download_file_to_path(&url, &sha512, &temp_path)
            .await
            .expect("download verified file");

        assert_eq!(fs::read(&temp_path).expect("read temp file"), body);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_maps_429_before_creating_temp_file() {
        let url = spawn_response_server(
            "429 Too Many Requests",
            vec![("X-Ratelimit-Reset".to_string(), "7".to_string())],
            vec![b'x'; RATE_LIMIT_BODY_LIMIT + 64],
        )
        .await;
        let root = test_root("download-rate-limited");
        let temp_path = root.join("nested").join("sodium.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path(&url, "", &temp_path)
            .await
            .expect_err("429 should return typed rate-limit error");

        match error {
            ModrinthError::RateLimited {
                reset_after_seconds,
                body,
            } => {
                assert_eq!(reset_after_seconds, Some(7));
                assert_eq!(body.len(), RATE_LIMIT_BODY_LIMIT);
            }
            other => panic!("expected rate-limit error, got {other:?}"),
        }
        assert!(!temp_path.exists());
        assert!(!temp_path.parent().expect("temp path parent").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_removes_temp_file_on_sha512_mismatch() {
        let url = spawn_file_server(b"managed-jar".to_vec()).await;
        let root = test_root("download-stream-mismatch");
        let temp_path = root.join("sodium.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path(&url, "not-the-right-hash", &temp_path)
            .await
            .expect_err("hash mismatch should fail");

        assert!(matches!(error, ModrinthError::HashMismatch { .. }));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    async fn spawn_file_server(body: Vec<u8>) -> String {
        let base_url = spawn_response_server(
            "200 OK",
            vec![(
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            )],
            body,
        )
        .await;
        format!("{base_url}/files/sodium.jar")
    }

    async fn spawn_response_server(
        status: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind response server");
        let addr = listener.local_addr().expect("response server addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                let headers = headers.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 1024];
                    loop {
                        let Ok(read) = stream.read(&mut buf).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 8192 {
                            return;
                        }
                    }

                    let mut response = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n",
                        body.len(),
                    );
                    for (name, value) in headers {
                        response.push_str(&name);
                        response.push_str(": ");
                        response.push_str(&value);
                        response.push_str("\r\n");
                    }
                    response.push_str("\r\n");

                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    let midpoint = body.len() / 2;
                    if stream.write_all(&body[..midpoint]).await.is_err() {
                        return;
                    }
                    let _ = stream.write_all(&body[midpoint..]).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-performance-modrinth-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }
}
