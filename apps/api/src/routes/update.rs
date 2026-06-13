use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::{
    sync::OnceLock,
    time::{Duration, SystemTime},
};

const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/mateoltd/croopor/releases/latest";
const GITHUB_RELEASE_PAGE_TAG_PREFIX: &str = "https://github.com/mateoltd/croopor/releases/tag/";
const GITHUB_RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/mateoltd/croopor/releases/download/";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const UPDATE_CHECK_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const UPDATE_CHECK_UNAVAILABLE_MESSAGE: &str = "update check unavailable";
const MAX_UPDATE_RELEASE_BYTES: u64 = 512 << 10;

type ApiErrorResponse = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Serialize)]
struct UpdateResponse {
    current_version: String,
    latest_version: String,
    available: bool,
    platform: String,
    arch: String,
    kind: &'static str,
    notes_url: String,
    action_url: String,
    checksum_url: Option<String>,
    action_label: String,
    checked_at: String,
}

#[derive(Debug, Deserialize)]
struct GithubLatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/update", get(handle_update))
}

async fn handle_update(
    State(state): State<AppState>,
) -> Result<Json<UpdateResponse>, ApiErrorResponse> {
    let current_version = state.version().to_string();
    let checked_at = timestamp_utc();

    update_response_from_release_fetch(
        &current_version,
        &checked_at,
        fetch_latest_release(&current_version).await,
    )
    .map(Json)
}

async fn fetch_latest_release(
    current_version: &str,
) -> Result<GithubLatestRelease, UpdateFetchError> {
    fetch_latest_release_from_url(GITHUB_LATEST_RELEASE_URL, current_version).await
}

async fn fetch_latest_release_from_url(
    url: &str,
    current_version: &str,
) -> Result<GithubLatestRelease, UpdateFetchError> {
    let response = update_http_client()
        .get(url)
        .header(USER_AGENT, format!("Croopor/{current_version}"))
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(UpdateFetchError::Request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateFetchError::HttpStatus(status));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_UPDATE_RELEASE_BYTES)
    {
        return Err(UpdateFetchError::TooLarge);
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(UpdateFetchError::Request)?;
        if body.len() as u64 + chunk.len() as u64 > MAX_UPDATE_RELEASE_BYTES {
            return Err(UpdateFetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice::<GithubLatestRelease>(&body).map_err(UpdateFetchError::Json)
}

#[derive(Debug)]
enum UpdateFetchError {
    Request(reqwest::Error),
    HttpStatus(StatusCode),
    Json(serde_json::Error),
    TooLarge,
}

impl std::fmt::Display for UpdateFetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) => write!(formatter, "request failed: {error}"),
            Self::HttpStatus(status) => write!(formatter, "HTTP {status}"),
            Self::Json(error) => write!(formatter, "parse failed: {error}"),
            Self::TooLarge => write!(formatter, "response too large"),
        }
    }
}

fn update_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(UPDATE_CHECK_CONNECT_TIMEOUT)
                .timeout(UPDATE_CHECK_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

fn fallback_response(current_version: &str, checked_at: &str) -> UpdateResponse {
    UpdateResponse {
        current_version: current_version.to_string(),
        latest_version: current_version.to_string(),
        available: false,
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        kind: "none",
        notes_url: String::new(),
        action_url: String::new(),
        checksum_url: None,
        action_label: String::new(),
        checked_at: checked_at.to_string(),
    }
}

fn release_response(
    current_version: &str,
    checked_at: &str,
    release: GithubLatestRelease,
) -> UpdateResponse {
    release_response_for_platform(
        current_version,
        checked_at,
        release,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn release_response_for_platform(
    current_version: &str,
    checked_at: &str,
    release: GithubLatestRelease,
    os: &str,
    arch: &str,
) -> UpdateResponse {
    let latest_version = normalized_version(&release.tag_name);
    let Some(release_url) = sane_release_page_url(&release.html_url, &latest_version) else {
        return fallback_response(current_version, checked_at);
    };
    if !is_version_greater(&latest_version, current_version) {
        return fallback_response(current_version, checked_at);
    }

    let asset = matching_release_asset(&release.assets, &latest_version, os, arch);
    if let Some(asset) = asset {
        return UpdateResponse {
            current_version: current_version.to_string(),
            latest_version,
            available: true,
            platform: os.to_string(),
            arch: arch.to_string(),
            kind: "release-asset",
            notes_url: release_url,
            action_url: asset.url,
            checksum_url: asset.checksum_url,
            action_label: "Download update".to_string(),
            checked_at: checked_at.to_string(),
        };
    }

    UpdateResponse {
        current_version: current_version.to_string(),
        latest_version,
        available: true,
        platform: os.to_string(),
        arch: arch.to_string(),
        kind: "release-page",
        notes_url: release_url.clone(),
        action_url: release_url,
        checksum_url: None,
        action_label: "Open release".to_string(),
        checked_at: checked_at.to_string(),
    }
}

fn update_response_from_release_fetch<E>(
    current_version: &str,
    checked_at: &str,
    result: Result<GithubLatestRelease, E>,
) -> Result<UpdateResponse, ApiErrorResponse> {
    result
        .map(|release| release_response(current_version, checked_at, release))
        .map_err(|_| update_unavailable_response())
}

fn update_unavailable_response() -> ApiErrorResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": UPDATE_CHECK_UNAVAILABLE_MESSAGE })),
    )
}

fn normalized_version(version: &str) -> String {
    version.trim().trim_start_matches(['v', 'V']).to_string()
}

fn sane_release_page_url(url: &str, latest_version: &str) -> Option<String> {
    let trimmed = url.trim();
    let tag = trimmed.strip_prefix(GITHUB_RELEASE_PAGE_TAG_PREFIX)?;
    if trimmed != url
        || tag.is_empty()
        || tag.contains(['/', '?', '#'])
        || normalized_version(tag) != latest_version
    {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
fn matching_release_asset_url(
    assets: &[GithubReleaseAsset],
    latest_version: &str,
    os: &str,
    arch: &str,
) -> Option<String> {
    matching_release_asset(assets, latest_version, os, arch).map(|asset| asset.url)
}

struct ReleaseAssetSelection {
    url: String,
    checksum_url: Option<String>,
}

fn matching_release_asset(
    assets: &[GithubReleaseAsset],
    latest_version: &str,
    os: &str,
    arch: &str,
) -> Option<ReleaseAssetSelection> {
    let expected_name = release_asset_name(latest_version, os, arch)?;
    let url = assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .find_map(|asset| sane_release_asset_url(&asset.browser_download_url, &expected_name))?;
    let checksum_name = format!("{expected_name}.sha256");
    let checksum_url = assets
        .iter()
        .filter(|asset| asset.name == checksum_name)
        .find_map(|asset| sane_release_asset_url(&asset.browser_download_url, &checksum_name));

    Some(ReleaseAssetSelection { url, checksum_url })
}

fn release_asset_name(latest_version: &str, os: &str, arch: &str) -> Option<String> {
    let platform = match os {
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let archive_ext = match os {
        "linux" => "tar.gz",
        "windows" => "zip",
        _ => return None,
    };
    let package_arch = match arch {
        "x86_64" => "amd64",
        _ => return None,
    };

    Some(format!(
        "croopor-{platform}-{package_arch}-{latest_version}.{archive_ext}"
    ))
}

fn release_asset_version_from_name(name: &str) -> Option<&str> {
    let archive_name = name.strip_suffix(".sha256").unwrap_or(name);
    let archive_suffix = archive_name.rsplit_once('-')?.1;
    archive_suffix
        .strip_suffix(".tar.gz")
        .or_else(|| archive_suffix.strip_suffix(".zip"))
}

fn sane_release_asset_url(url: &str, expected_name: &str) -> Option<String> {
    let trimmed = url.trim();
    let download_path = trimmed.strip_prefix(GITHUB_RELEASE_DOWNLOAD_PREFIX)?;
    if trimmed != url {
        return None;
    }
    let (tag, filename) = download_path.split_once('/')?;
    if filename != expected_name {
        return None;
    }
    let expected_version = release_asset_version_from_name(expected_name)?;
    let expected_v_tag = format!("v{expected_version}");
    let expected_upper_v_tag = format!("V{expected_version}");
    if tag != expected_version && tag != expected_v_tag && tag != expected_upper_v_tag {
        return None;
    }

    Some(trimmed.to_string())
}

fn is_version_greater(candidate: &str, current: &str) -> bool {
    let Some(candidate_parts) = parse_numeric_version(candidate) else {
        return false;
    };
    let Some(current_parts) = parse_numeric_version(current) else {
        return false;
    };

    let width = candidate_parts.len().max(current_parts.len());
    for index in 0..width {
        let candidate_part = candidate_parts.get(index).copied().unwrap_or(0);
        let current_part = current_parts.get(index).copied().unwrap_or(0);
        if candidate_part > current_part {
            return true;
        }
        if candidate_part < current_part {
            return false;
        }
    }
    false
}

fn parse_numeric_version(version: &str) -> Option<Vec<u64>> {
    let normalized = normalized_version(version);
    if normalized.is_empty() {
        return None;
    }

    normalized
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            part.parse::<u64>().ok()
        })
        .collect()
}

fn timestamp_utc() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn release_asset(name: &str, browser_download_url: &str) -> GithubReleaseAsset {
        GithubReleaseAsset {
            name: name.to_string(),
            browser_download_url: browser_download_url.to_string(),
        }
    }

    #[test]
    fn version_comparison_strips_leading_v_and_compares_numeric_parts() {
        assert!(is_version_greater("v1.2.4", "1.2.3"));
        assert!(is_version_greater("1.10.0", "1.9.9"));
        assert!(!is_version_greater("1.2.0", "1.2"));
        assert!(!is_version_greater("1.2.3", "1.2.3"));
        assert!(!is_version_greater("1.2.2", "1.2.3"));
    }

    #[test]
    fn version_comparison_rejects_unknown_suffixes() {
        assert!(!is_version_greater("1.2.4-beta", "1.2.3"));
        assert!(!is_version_greater("1.2.4+build", "1.2.3"));
        assert!(!is_version_greater("release-1.2.4", "1.2.3"));
        assert!(!is_version_greater("1.2.4", "dev"));
    }

    #[test]
    fn release_response_maps_available_release_page() {
        let response = release_response(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: Vec::new(),
            },
        );

        assert!(response.available);
        assert_eq!(response.latest_version, "1.2.4");
        assert_eq!(response.kind, "release-page");
        assert_eq!(response.notes_url, response.action_url);
        assert_eq!(response.action_label, "Open release");
    }

    #[test]
    fn release_response_falls_back_for_non_greater_or_unusable_release() {
        let same = release_response(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.3".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.3".to_string(),
                assets: Vec::new(),
            },
        );
        assert!(!same.available);
        assert_eq!(same.latest_version, "1.2.3");
        assert_eq!(same.kind, "none");

        let suffix = release_response(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4-beta".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4-beta"
                    .to_string(),
                assets: Vec::new(),
            },
        );
        assert!(!suffix.available);
        assert_eq!(suffix.latest_version, "1.2.3");
        assert_eq!(suffix.kind, "none");

        let wrong_url = release_response(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://example.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: Vec::new(),
            },
        );
        assert!(!wrong_url.available);
        assert_eq!(wrong_url.latest_version, "1.2.3");
        assert_eq!(wrong_url.kind, "none");

        let mismatched_page_tag = release_response(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.5".to_string(),
                assets: Vec::new(),
            },
        );
        assert!(!mismatched_page_tag.available);
        assert_eq!(mismatched_page_tag.latest_version, "1.2.3");
        assert_eq!(mismatched_page_tag.kind, "none");
    }

    #[test]
    fn update_fetch_failure_maps_to_service_unavailable_error() {
        let error =
            update_response_from_release_fetch::<()>("1.2.3", "2026-01-01T00:00:00Z", Err(()))
                .expect_err("fetch failure should not become no-update success");

        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": UPDATE_CHECK_UNAVAILABLE_MESSAGE })
        );
    }

    #[tokio::test]
    async fn update_fetch_reads_bounded_release_json() {
        let url =
            serve_update_release_json(200, sample_release_json("v1.2.4").into_bytes(), None).await;

        let release = fetch_latest_release_from_url(&url, "1.2.3")
            .await
            .expect("release json");

        assert_eq!(release.tag_name, "v1.2.4");
        assert_eq!(
            release.html_url,
            "https://github.com/mateoltd/croopor/releases/tag/v1.2.4"
        );
    }

    #[tokio::test]
    async fn update_fetch_rejects_http_errors() {
        let url = serve_update_release_json(503, b"unavailable".to_vec(), None).await;

        let error = fetch_latest_release_from_url(&url, "1.2.3")
            .await
            .expect_err("HTTP error should fail");

        match error {
            UpdateFetchError::HttpStatus(status) => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE)
            }
            error => panic!("expected HTTP status error, got {error:?}"),
        }
    }

    #[tokio::test]
    async fn update_fetch_rejects_oversized_content_length() {
        let url =
            serve_update_release_json(200, b"{}".to_vec(), Some(MAX_UPDATE_RELEASE_BYTES + 1))
                .await;

        let error = fetch_latest_release_from_url(&url, "1.2.3")
            .await
            .expect_err("oversized release response should fail");

        assert!(matches!(error, UpdateFetchError::TooLarge));
    }

    #[test]
    fn linux_asset_selection_matches_packaged_archive() {
        let asset_url = matching_release_asset_url(
            &[
                release_asset(
                    "croopor-windows-amd64-1.2.4.zip",
                    "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-windows-amd64-1.2.4.zip",
                ),
                release_asset(
                    "croopor-linux-amd64-1.2.4.tar.gz",
                    "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
                ),
            ],
            "1.2.4",
            "linux",
            "x86_64",
        )
        .expect("linux x86_64 should select tar.gz asset");

        assert_eq!(
            asset_url,
            "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz"
        );
    }

    #[test]
    fn windows_asset_selection_matches_packaged_archive() {
        let asset_url = matching_release_asset_url(
            &[
                release_asset(
                    "croopor-linux-amd64-1.2.4.tar.gz",
                    "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
                ),
                release_asset(
                    "croopor-windows-amd64-1.2.4.zip",
                    "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-windows-amd64-1.2.4.zip",
                ),
            ],
            "1.2.4",
            "windows",
            "x86_64",
        )
        .expect("windows x86_64 should select zip asset");

        assert_eq!(
            asset_url,
            "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-windows-amd64-1.2.4.zip"
        );
    }

    #[test]
    fn asset_selection_rejects_missing_or_unsafe_assets() {
        let missing = matching_release_asset_url(
            &[release_asset(
                "croopor-linux-amd64-1.2.3.tar.gz",
                "https://github.com/mateoltd/croopor/releases/download/v1.2.3/croopor-linux-amd64-1.2.3.tar.gz",
            )],
            "1.2.4",
            "linux",
            "x86_64",
        );
        assert!(missing.is_none());

        let unsafe_url = matching_release_asset_url(
            &[release_asset(
                "croopor-linux-amd64-1.2.4.tar.gz",
                "https://example.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
            )],
            "1.2.4",
            "linux",
            "x86_64",
        );
        assert!(unsafe_url.is_none());

        let mismatched_filename = matching_release_asset_url(
            &[release_asset(
                "croopor-linux-amd64-1.2.4.tar.gz",
                "https://github.com/mateoltd/croopor/releases/download/v1.2.4/other.tar.gz",
            )],
            "1.2.4",
            "linux",
            "x86_64",
        );
        assert!(mismatched_filename.is_none());

        let mismatched_release_tag = matching_release_asset_url(
            &[release_asset(
                "croopor-linux-amd64-1.2.4.tar.gz",
                "https://github.com/mateoltd/croopor/releases/download/v1.2.5/croopor-linux-amd64-1.2.4.tar.gz",
            )],
            "1.2.4",
            "linux",
            "x86_64",
        );
        assert!(mismatched_release_tag.is_none());

        let unsupported_arch = matching_release_asset_url(
            &[release_asset(
                "croopor-linux-amd64-1.2.4.tar.gz",
                "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
            )],
            "1.2.4",
            "linux",
            "aarch64",
        );
        assert!(unsupported_arch.is_none());
    }

    #[test]
    fn release_response_prefers_matching_asset_over_release_page() {
        let response = release_response_for_platform(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: vec![release_asset(
                    "croopor-linux-amd64-1.2.4.tar.gz",
                    "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
                )],
            },
            "linux",
            "x86_64",
        );

        assert_eq!(response.kind, "release-asset");
        assert_eq!(response.action_label, "Download update");
        assert_eq!(response.checksum_url, None);
        assert_eq!(
            response.notes_url,
            "https://github.com/mateoltd/croopor/releases/tag/v1.2.4"
        );
        assert_eq!(
            response.action_url,
            "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz"
        );
    }

    #[test]
    fn release_response_includes_matching_checksum_sidecar() {
        let response = release_response_for_platform(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: vec![
                    release_asset(
                        "croopor-windows-amd64-1.2.4.zip",
                        "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-windows-amd64-1.2.4.zip",
                    ),
                    release_asset(
                        "croopor-windows-amd64-1.2.4.zip.sha256",
                        "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-windows-amd64-1.2.4.zip.sha256",
                    ),
                ],
            },
            "windows",
            "x86_64",
        );

        assert_eq!(response.kind, "release-asset");
        assert_eq!(
            response.checksum_url.as_deref(),
            Some(
                "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-windows-amd64-1.2.4.zip.sha256"
            )
        );
    }

    #[test]
    fn release_response_omits_missing_or_unsafe_checksum_sidecar() {
        let wrong_host = release_response_for_platform(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: vec![
                    release_asset(
                        "croopor-linux-amd64-1.2.4.tar.gz",
                        "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
                    ),
                    release_asset(
                        "croopor-linux-amd64-1.2.4.tar.gz.sha256",
                        "https://example.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz.sha256",
                    ),
                ],
            },
            "linux",
            "x86_64",
        );
        assert_eq!(wrong_host.kind, "release-asset");
        assert_eq!(wrong_host.checksum_url, None);

        let wrong_tag = release_response_for_platform(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: vec![
                    release_asset(
                        "croopor-linux-amd64-1.2.4.tar.gz",
                        "https://github.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
                    ),
                    release_asset(
                        "croopor-linux-amd64-1.2.4.tar.gz.sha256",
                        "https://github.com/mateoltd/croopor/releases/download/v1.2.5/croopor-linux-amd64-1.2.4.tar.gz.sha256",
                    ),
                ],
            },
            "linux",
            "x86_64",
        );
        assert_eq!(wrong_tag.kind, "release-asset");
        assert_eq!(wrong_tag.checksum_url, None);
    }

    #[test]
    fn release_response_falls_back_to_release_page_for_missing_or_unsafe_asset() {
        let missing = release_response_for_platform(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: Vec::new(),
            },
            "linux",
            "x86_64",
        );
        assert!(missing.available);
        assert_eq!(missing.kind, "release-page");
        assert_eq!(missing.action_label, "Open release");

        let unsafe_asset = release_response_for_platform(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://github.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: vec![release_asset(
                    "croopor-linux-amd64-1.2.4.tar.gz",
                    "https://example.com/mateoltd/croopor/releases/download/v1.2.4/croopor-linux-amd64-1.2.4.tar.gz",
                )],
            },
            "linux",
            "x86_64",
        );
        assert!(unsafe_asset.available);
        assert_eq!(unsafe_asset.kind, "release-page");
        assert_eq!(unsafe_asset.action_label, "Open release");
    }

    #[test]
    fn update_fetch_success_preserves_no_update_fallbacks() {
        let response = update_response_from_release_fetch::<()>(
            "1.2.3",
            "2026-01-01T00:00:00Z",
            Ok(GithubLatestRelease {
                tag_name: "v1.2.4".to_string(),
                html_url: "https://example.com/mateoltd/croopor/releases/tag/v1.2.4".to_string(),
                assets: Vec::new(),
            }),
        )
        .expect("unusable release URL should remain a successful no-update response");

        assert!(!response.available);
        assert_eq!(response.latest_version, "1.2.3");
        assert_eq!(response.kind, "none");
    }

    async fn serve_update_release_json(
        status: u16,
        body: Vec<u8>,
        content_length: Option<u64>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind update test server");
        let address = listener.local_addr().expect("update test server address");
        let content_length = content_length.unwrap_or(body.len() as u64);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept update request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let reason = if status == 200 { "OK" } else { "Error" };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write update response headers");
            socket
                .write_all(&body)
                .await
                .expect("write update response body");
        });
        format!("http://{address}/latest")
    }

    fn sample_release_json(tag: &str) -> String {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "html_url": "https://github.com/mateoltd/croopor/releases/tag/{tag}",
                "assets": []
            }}"#
        )
    }
}
