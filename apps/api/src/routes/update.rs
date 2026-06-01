use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/mateoltd/croopor/releases/latest";
const GITHUB_RELEASE_PAGE_PREFIX: &str = "https://github.com/mateoltd/croopor/releases/";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

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
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/update", get(handle_update))
}

async fn handle_update(State(state): State<AppState>) -> Json<UpdateResponse> {
    let current_version = state.version().to_string();
    let checked_at = timestamp_utc();
    let fallback = || fallback_response(&current_version, &checked_at);

    let response = match fetch_latest_release(&current_version).await {
        Ok(release) => release_response(&current_version, &checked_at, release),
        Err(_) => fallback(),
    };

    Json(response)
}

async fn fetch_latest_release(
    current_version: &str,
) -> Result<GithubLatestRelease, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(UPDATE_CHECK_TIMEOUT)
        .build()?;

    client
        .get(GITHUB_LATEST_RELEASE_URL)
        .header(USER_AGENT, format!("Croopor/{current_version}"))
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GithubLatestRelease>()
        .await
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
    let latest_version = normalized_version(&release.tag_name);
    let Some(release_url) = sane_release_page_url(&release.html_url) else {
        return fallback_response(current_version, checked_at);
    };
    if !is_version_greater(&latest_version, current_version) {
        return fallback_response(current_version, checked_at);
    }

    UpdateResponse {
        current_version: current_version.to_string(),
        latest_version,
        available: true,
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        kind: "release-page",
        notes_url: release_url.clone(),
        action_url: release_url,
        action_label: "Open release".to_string(),
        checked_at: checked_at.to_string(),
    }
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
            },
        );
        assert!(!wrong_url.available);
        assert_eq!(wrong_url.latest_version, "1.2.3");
        assert_eq!(wrong_url.kind, "none");
    }
}
