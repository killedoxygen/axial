use crate::MANAGED_ARTIFACT_MAX_BYTES;
use hex::encode;
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

const USER_AGENT: &str = "axial/0.4.0-alpha (github.com/mateoltd/axial)";
const RATE_LIMIT_BODY_LIMIT: usize = 4096;
const MAX_MODRINTH_VERSION_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MODRINTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MODRINTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MODRINTH_CLIENT_MAX_IDLE_PER_HOST: usize = 4;
const MODRINTH_CLIENT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const MODRINTH_CLIENT_TCP_KEEPALIVE: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum ModrinthError {
    #[error("modrinth request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("modrinth response parse failed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("modrinth download failed: {0}")]
    Io(#[from] io::Error),
    #[error("modrinth API rate limited; reset after {reset_after_seconds:?} seconds: {body}")]
    RateLimited {
        reset_after_seconds: Option<u64>,
        body: String,
    },
    #[error("modrinth API returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("modrinth response too large")]
    ResponseTooLarge,
    #[error("hash mismatch: expected {expected} got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("download size exceeded: expected {expected} bytes got at least {actual} bytes")]
    SizeExceeded { expected: u64, actual: u64 },
    #[error("download size mismatch: expected {expected} bytes got {actual} bytes")]
    SizeMismatch { expected: u64, actual: u64 },
}

#[derive(Debug, Clone)]
pub struct ModrinthClient {
    client: Client,
    base_url: String,
}

impl ModrinthClient {
    pub fn new() -> Self {
        Self {
            client: modrinth_http_client(),
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
            let body = bounded_response_text(response, RATE_LIMIT_BODY_LIMIT).await;
            return Err(ModrinthError::Http { status, body });
        }

        let versions = serde_json::from_slice::<Vec<Version>>(
            &bounded_version_response_body(response).await?,
        )?;
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

    pub(crate) async fn download_file_to_path(
        &self,
        url: &str,
        expected_sha512: &str,
        expected_size: Option<u64>,
        temp_path: &Path,
    ) -> Result<ManagedDownloadTemp, ModrinthError> {
        self.download_file_to_path_with_limit(
            url,
            expected_sha512,
            expected_size,
            temp_path,
            MANAGED_ARTIFACT_MAX_BYTES,
        )
        .await
    }

    async fn download_file_to_path_with_limit(
        &self,
        url: &str,
        expected_sha512: &str,
        expected_size: Option<u64>,
        temp_path: &Path,
        absolute_max_bytes: u64,
    ) -> Result<ManagedDownloadTemp, ModrinthError> {
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limited_error(response).await);
            }

            let status = response.status().as_u16();
            let body = bounded_response_text(response, RATE_LIMIT_BODY_LIMIT).await;
            return Err(ModrinthError::Http { status, body });
        }
        let max_bytes = expected_size
            .unwrap_or(absolute_max_bytes)
            .min(absolute_max_bytes);
        if let Some(actual) = response.content_length()
            && actual > max_bytes
        {
            return Err(ModrinthError::SizeExceeded {
                expected: max_bytes,
                actual,
            });
        }

        if let Some(parent) = temp_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let create_path = temp_path.to_path_buf();
        let created = tokio::task::spawn_blocking(move || CreatedDownloadTemp::create(create_path))
            .await
            .map_err(|_| {
                ModrinthError::Io(io::Error::other(
                    "managed download temp creation task stopped",
                ))
            })??;
        let (output, owned_temp) = created.into_parts()?;
        let mut output = tokio::fs::File::from_std(output);
        let result = async {
            let mut hasher = Sha512::new();
            let mut actual_size = 0_u64;
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                actual_size = actual_size.saturating_add(chunk.len() as u64);
                if actual_size > max_bytes {
                    return Err(ModrinthError::SizeExceeded {
                        expected: max_bytes,
                        actual: actual_size,
                    });
                }
                hasher.update(&chunk);
                output.write_all(&chunk).await?;
            }
            output.flush().await?;
            Ok::<(String, u64), ModrinthError>((encode(hasher.finalize()), actual_size))
        }
        .await;

        let (actual, actual_size) = result?;

        if let Some(expected) = expected_size
            && actual_size != expected
        {
            return Err(ModrinthError::SizeMismatch {
                expected,
                actual: actual_size,
            });
        }

        if !expected_sha512.trim().is_empty() && !actual.eq_ignore_ascii_case(expected_sha512) {
            return Err(ModrinthError::HashMismatch {
                expected: expected_sha512.to_string(),
                actual,
            });
        }
        Ok(owned_temp.with_sha512(actual))
    }
}

#[derive(Debug)]
pub(crate) struct ManagedDownloadTemp {
    path: std::path::PathBuf,
    sha512: String,
    identity: crate::file_identity::FileIdentity,
    armed: bool,
}

impl ManagedDownloadTemp {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sha512(&self) -> &str {
        &self.sha512
    }

    pub(crate) fn owns_path(&self, path: &Path, expected_len: u64) -> bool {
        crate::file_identity::revalidate(path, self.identity, expected_len).is_ok()
    }

    pub(crate) async fn owns_path_async(&self, path: &Path, expected_len: u64) -> bool {
        crate::file_identity::revalidate_async(path, self.identity, expected_len)
            .await
            .is_ok()
    }

    fn with_sha512(mut self, sha512: String) -> Self {
        self.sha512 = sha512;
        self
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    pub(crate) async fn cleanup(mut self) -> Result<(), io::Error> {
        let current = match tokio::fs::symlink_metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.disarm();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if !self.owns_path_async(&self.path, current.len()).await {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed download temp identity changed before cleanup",
            ));
        }
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => {
                self.disarm();
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.disarm();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

struct CreatedDownloadTemp {
    file: Option<std::fs::File>,
    path: std::path::PathBuf,
    identity: crate::file_identity::FileIdentity,
    armed: bool,
}

impl CreatedDownloadTemp {
    fn create(path: std::path::PathBuf) -> Result<Self, io::Error> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let identity = crate::file_identity::from_file(&file)?;
        Ok(Self {
            file: Some(file),
            path,
            identity,
            armed: true,
        })
    }

    fn into_parts(mut self) -> Result<(std::fs::File, ManagedDownloadTemp), io::Error> {
        let file = self
            .file
            .take()
            .expect("created download temp owns its file");
        let identity = crate::file_identity::from_file(&file)?;
        if identity != self.identity {
            return Err(io::Error::other(
                "managed download temp identity changed after creation",
            ));
        }
        self.armed = false;
        let guard = ManagedDownloadTemp {
            path: self.path.clone(),
            sha512: String::new(),
            identity,
            armed: true,
        };
        Ok((file, guard))
    }
}

impl Drop for CreatedDownloadTemp {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.armed
            && std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
                metadata.file_type().is_file()
                    && crate::file_identity::revalidate(&self.path, self.identity, metadata.len())
                        .is_ok()
            })
            && let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!("managed download temp cleanup failed");
        }
    }
}

impl Drop for ManagedDownloadTemp {
    fn drop(&mut self) {
        if self.armed {
            let removable = std::fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| self.owns_path(&self.path, metadata.len()));
            if removable
                && let Err(error) = std::fs::remove_file(&self.path)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!("managed download temp cleanup failed");
            }
        }
    }
}

fn modrinth_http_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(MODRINTH_CONNECT_TIMEOUT)
                .timeout(MODRINTH_REQUEST_TIMEOUT)
                .pool_max_idle_per_host(MODRINTH_CLIENT_MAX_IDLE_PER_HOST)
                .pool_idle_timeout(MODRINTH_CLIENT_POOL_IDLE_TIMEOUT)
                .tcp_keepalive(MODRINTH_CLIENT_TCP_KEEPALIVE)
                .build()
                .unwrap_or_else(|_| Client::new())
        })
        .clone()
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
    #[serde(default)]
    pub size: Option<u64>,
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

async fn bounded_version_response_body(mut response: Response) -> Result<Vec<u8>, ModrinthError> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_MODRINTH_VERSION_RESPONSE_BYTES as u64)
    {
        return Err(ModrinthError::ResponseTooLarge);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_MODRINTH_VERSION_RESPONSE_BYTES {
            return Err(ModrinthError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
    async fn list_versions_rejects_oversized_content_length() {
        let base_url = spawn_response_server_with_content_length(
            "200 OK",
            vec![("content-type".to_string(), "application/json".to_string())],
            b"[]".to_vec(),
            Some(MAX_MODRINTH_VERSION_RESPONSE_BYTES + 1),
        )
        .await;
        let client = ModrinthClient::new_with_base_url(base_url);

        let error = client
            .list_versions("sodium", &["1.21.1".to_string()], &["fabric".to_string()])
            .await
            .expect_err("oversized content-length should fail");

        assert!(matches!(error, ModrinthError::ResponseTooLarge));
    }

    #[tokio::test]
    async fn list_versions_rejects_stream_past_limit_without_content_length() {
        let base_url = spawn_response_server_with_content_length(
            "200 OK",
            vec![("content-type".to_string(), "application/json".to_string())],
            vec![b' '; MAX_MODRINTH_VERSION_RESPONSE_BYTES + 1],
            None,
        )
        .await;
        let client = ModrinthClient::new_with_base_url(base_url);

        let error = client
            .list_versions("sodium", &["1.21.1".to_string()], &["fabric".to_string()])
            .await
            .expect_err("oversized stream should fail");

        assert!(matches!(error, ModrinthError::ResponseTooLarge));
    }

    #[tokio::test]
    async fn list_versions_bounds_non_429_error_body() {
        let base_url = spawn_response_server(
            "502 Bad Gateway",
            vec![("content-type".to_string(), "text/plain".to_string())],
            vec![b'x'; RATE_LIMIT_BODY_LIMIT + 64],
        )
        .await;
        let client = ModrinthClient::new_with_base_url(base_url);

        let error = client
            .list_versions("sodium", &["1.21.1".to_string()], &["fabric".to_string()])
            .await
            .expect_err("provider error should fail");

        match error {
            ModrinthError::Http { status, body } => {
                assert_eq!(status, 502);
                assert_eq!(body.len(), RATE_LIMIT_BODY_LIMIT);
            }
            other => panic!("expected HTTP error, got {other:?}"),
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

        let _owned_temp = client
            .download_file_to_path(&url, &sha512, Some(body.len() as u64), &temp_path)
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
            .download_file_to_path(&url, "", None, &temp_path)
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
    async fn download_file_to_path_bounds_non_429_error_body() {
        let url = spawn_response_server(
            "502 Bad Gateway",
            vec![("content-type".to_string(), "text/plain".to_string())],
            vec![b'x'; RATE_LIMIT_BODY_LIMIT + 64],
        )
        .await;
        let root = test_root("download-http-error-body-limit");
        let temp_path = root.join("nested").join("sodium.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path(&url, "", None, &temp_path)
            .await
            .expect_err("provider error should fail");

        match error {
            ModrinthError::Http { status, body } => {
                assert_eq!(status, 502);
                assert_eq!(body.len(), RATE_LIMIT_BODY_LIMIT);
            }
            other => panic!("expected HTTP error, got {other:?}"),
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
            .download_file_to_path(&url, "not-the-right-hash", None, &temp_path)
            .await
            .expect_err("hash mismatch should fail");

        assert!(matches!(error, ModrinthError::HashMismatch { .. }));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_rejects_oversized_content_length_before_temp_file() {
        let url = spawn_response_server_with_content_length(
            "200 OK",
            vec![(
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            )],
            b"oversized".to_vec(),
            Some(64),
        )
        .await;
        let root = test_root("download-oversized-content-length");
        let temp_path = root.join("nested").join("sodium.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path(&url, "", Some(8), &temp_path)
            .await
            .expect_err("oversized content-length should fail");

        assert!(matches!(
            error,
            ModrinthError::SizeExceeded {
                expected: 8,
                actual: 64
            }
        ));
        assert!(!temp_path.exists());
        assert!(!temp_path.parent().expect("temp path parent").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_rejects_stream_past_expected_size_and_removes_temp_file() {
        let url = spawn_response_server_with_content_length(
            "200 OK",
            vec![(
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            )],
            b"managed-jar".to_vec(),
            None,
        )
        .await;
        let root = test_root("download-stream-oversized");
        let temp_path = root.join("sodium.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path(&url, "", Some(4), &temp_path)
            .await
            .expect_err("stream exceeding expected size should fail");

        assert!(matches!(
            error,
            ModrinthError::SizeExceeded {
                expected: 4,
                actual
            } if actual > 4
        ));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_rejects_truncated_body_without_sha512() {
        let url = spawn_response_server_with_content_length(
            "200 OK",
            Vec::new(),
            b"short".to_vec(),
            None,
        )
        .await;
        let root = test_root("download-truncated-provider-size");
        let temp_path = root.join("managed.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path(&url, "", Some(8), &temp_path)
            .await
            .expect_err("body shorter than provider size should fail without a hash");

        assert!(matches!(
            error,
            ModrinthError::SizeMismatch {
                expected: 8,
                actual: 5
            }
        ));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_enforces_absolute_content_length_without_provider_size() {
        let url = spawn_response_server_with_content_length(
            "200 OK",
            Vec::new(),
            b"oversized".to_vec(),
            Some(64),
        )
        .await;
        let root = test_root("download-absolute-content-length");
        let temp_path = root.join("managed.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path_with_limit(&url, "", None, &temp_path, 8)
            .await
            .expect_err("absolute content-length limit should apply without provider size");

        assert!(matches!(
            error,
            ModrinthError::SizeExceeded {
                expected: 8,
                actual: 64
            }
        ));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_enforces_absolute_stream_limit_without_provider_size() {
        let url = spawn_response_server_with_content_length(
            "200 OK",
            Vec::new(),
            b"managed-jar".to_vec(),
            None,
        )
        .await;
        let root = test_root("download-absolute-stream");
        let temp_path = root.join("managed.jar.tmp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path_with_limit(&url, "", None, &temp_path, 8)
            .await
            .expect_err("absolute stream limit should apply without provider size");

        assert!(matches!(
            error,
            ModrinthError::SizeExceeded {
                expected: 8,
                actual
            } if actual > 8
        ));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_to_path_rejects_preexisting_temp_without_truncating_it() {
        let url = spawn_file_server(b"managed-jar".to_vec()).await;
        let root = test_root("download-preexisting-temp");
        let temp_path = root.join("managed.jar.tmp");
        fs::write(&temp_path, b"preexisting").expect("write preexisting temp");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path_with_limit(&url, "", None, &temp_path, 64)
            .await
            .expect_err("exclusive temp creation should reject a preexisting file");

        assert!(matches!(error, ModrinthError::Io(_)));
        assert_eq!(
            fs::read(&temp_path).expect("read preexisting temp"),
            b"preexisting"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_download_removes_only_its_exclusively_created_temp() {
        let url = spawn_slow_file_server().await;
        let root = test_root("download-cancelled-owned-temp");
        let temp_path = root.join("managed.jar.tmp");
        let task_temp_path = temp_path.clone();
        let client = ModrinthClient::new();
        let task = tokio::spawn(async move {
            client
                .download_file_to_path_with_limit(&url, "", None, &task_temp_path, 64)
                .await
        });

        for _ in 0..100 {
            if temp_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            temp_path.exists(),
            "download should own its temp before cancellation"
        );

        task.abort();
        let _ = task.await;

        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_file_to_path_rejects_stale_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let body = b"managed-jar".to_vec();
        let url = spawn_file_server(body.clone()).await;
        let root = test_root("download-stale-symlink");
        let victim = root.join("victim.jar");
        let temp_path = root.join("managed.jar.tmp");
        fs::write(&victim, b"user-owned").expect("write symlink victim");
        symlink(&victim, &temp_path).expect("create stale temp symlink");
        let client = ModrinthClient::new();

        let error = client
            .download_file_to_path_with_limit(&url, "", None, &temp_path, 64)
            .await
            .expect_err("fresh exclusive temp must reject a stale symlink");

        assert!(matches!(error, ModrinthError::Io(_)));
        assert_eq!(fs::read(&victim).expect("read victim"), b"user-owned");
        assert!(
            fs::symlink_metadata(&temp_path)
                .expect("temp metadata")
                .file_type()
                .is_symlink()
        );
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

    async fn spawn_slow_file_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slow response server");
        let addr = listener.local_addr().expect("slow response server addr");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\na")
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = stream.write_all(b"managed-jar").await;
        });
        format!("http://{addr}/files/slow.jar")
    }

    async fn spawn_response_server(
        status: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> String {
        spawn_response_server_with_content_length(status, headers, body.clone(), Some(body.len()))
            .await
    }

    async fn spawn_response_server_with_content_length(
        status: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        content_length: Option<usize>,
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
                let content_length = content_length;
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

                    let mut response = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
                    if let Some(content_length) = content_length {
                        response.push_str(&format!("content-length: {content_length}\r\n"));
                    }
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
            "axial-performance-modrinth-{name}-{}-{}",
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
