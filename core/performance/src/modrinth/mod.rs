use hex::encode;
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::io;
use std::time::Duration;
use thiserror::Error;

const USER_AGENT: &str = "croopor/0.3.1 (github.com/mateoltd/croopor)";

#[derive(Debug, Error)]
pub enum ModrinthError {
    #[error("modrinth request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("modrinth download failed: {0}")]
    Io(#[from] io::Error),
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

    pub async fn download_file(
        &self,
        url: &str,
        expected_sha512: &str,
    ) -> Result<Vec<u8>, ModrinthError> {
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ModrinthError::Http { status, body });
        }

        let bytes = response.bytes().await?;
        if !expected_sha512.is_empty() {
            let actual = encode(Sha512::digest(&bytes));
            if !actual.eq_ignore_ascii_case(expected_sha512) {
                return Err(ModrinthError::HashMismatch {
                    expected: expected_sha512.to_string(),
                    actual,
                });
            }
        }
        Ok(bytes.to_vec())
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
