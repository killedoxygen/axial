use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::{
    sync::OnceLock,
    time::{Duration, SystemTime},
};

const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/mateoltd/croopor/releases/latest";
const GITHUB_RELEASE_PAGE_PREFIX: &str = "https://github.com/mateoltd/croopor/releases/";
const GITHUB_RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/mateoltd/croopor/releases/download/";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const UPDATE_CHECK_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const UPDATE_CHECK_UNAVAILABLE_MESSAGE: &str = "update check unavailable";

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
) -> Result<GithubLatestRelease, reqwest::Error> {
    update_http_client()
        .get(GITHUB_LATEST_RELEASE_URL)
        .header(USER_AGENT, format!("Croopor/{current_version}"))
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GithubLatestRelease>()
        .await
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
    let Some(release_url) = sane_release_page_url(&release.html_url) else {
        return fallback_response(current_version, checked_at);
    };
    if !is_version_greater(&latest_version, current_version) {
        return fallback_response(current_version, checked_at);
    }

    let asset_url = matching_release_asset_url(&release.assets, &latest_version, os, arch);
    if let Some(asset_url) = asset_url {
        return UpdateResponse {
            current_version: current_version.to_string(),
            latest_version,
            available: true,
            platform: os.to_string(),
            arch: arch.to_string(),
            kind: "release-asset",
            notes_url: release_url,
            action_url: asset_url,
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

fn sane_release_page_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed != url || !trimmed.starts_with(GITHUB_RELEASE_PAGE_PREFIX) {
        return None;
    }
    Some(trimmed.to_string())
}

fn matching_release_asset_url(
    assets: &[GithubReleaseAsset],
    latest_version: &str,
    os: &str,
    arch: &str,
) -> Option<String> {
    let expected_name = release_asset_name(latest_version, os, arch)?;
    assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .find_map(|asset| sane_release_asset_url(&asset.browser_download_url, &expected_name))
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

fn sane_release_asset_url(url: &str, expected_name: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed != url
        || !trimmed.starts_with(GITHUB_RELEASE_DOWNLOAD_PREFIX)
        || !trimmed.ends_with(&format!("/{expected_name}"))
    {
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
}
