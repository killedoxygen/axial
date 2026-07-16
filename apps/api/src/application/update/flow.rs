use crate::state::{
    AppState, RequestProducerHandoff, UpdateApplyAdmissionError, UpdateFlowPhase,
    UpdateFlowSnapshot,
};
use axum::{Json, http::StatusCode};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use super::{ApiErrorResponse, fetch_latest_release, release_response, timestamp_utc};

const UPDATE_DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPDATE_DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_UPDATE_ASSET_BYTES: u64 = 256 << 20;
const MAX_UPDATE_CHECKSUM_BYTES: u64 = 4 << 10;
const MAX_STAGED_BINARY_BYTES: u64 = 256 << 20;

const DOWNLOAD_FAILED_MESSAGE: &str = "update download failed";
const CHECKSUM_FAILED_MESSAGE: &str = "update checksum did not match";
const ARCHIVE_FAILED_MESSAGE: &str = "update package could not be unpacked";
const STAGING_FAILED_MESSAGE: &str = "update could not be staged";
const APPLY_FAILED_MESSAGE: &str = "update could not be applied";
const RELEASE_CHANGED_MESSAGE: &str = "the latest release changed; check for updates again";
const UPDATE_BUSY_MESSAGE: &str =
    "finish downloads and close running games before applying the update";

#[derive(Debug, Deserialize)]
pub struct UpdateDownloadRequest {
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateFlowResponse {
    pub phase: &'static str,
    pub version: String,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub percent: Option<u8>,
    pub message: String,
    pub checked_at: String,
}

pub fn update_flow_state(state: &AppState) -> UpdateFlowResponse {
    flow_response(state.updater().snapshot())
}

pub(crate) async fn start_update_download(
    state: &AppState,
    request: UpdateDownloadRequest,
    handoff: RequestProducerHandoff,
) -> Result<Json<UpdateFlowResponse>, ApiErrorResponse> {
    let current_version = state.version().to_string();
    let checked_at = timestamp_utc();
    let release = fetch_latest_release(&current_version)
        .await
        .map_err(|_| flow_error(StatusCode::SERVICE_UNAVAILABLE, "update check unavailable"))?;
    let check = release_response(&current_version, &checked_at, release);

    if !check.available || check.kind != "release-asset" || check.latest_version != request.version
    {
        return Err(flow_error(StatusCode::CONFLICT, RELEASE_CHANGED_MESSAGE));
    }
    let Some(checksum_url) = check.checksum_url.clone() else {
        return Err(flow_error(
            StatusCode::CONFLICT,
            "this release does not support in-app updates",
        ));
    };
    let Some(asset_name) = asset_file_name(&check.action_url) else {
        return Err(flow_error(StatusCode::CONFLICT, RELEASE_CHANGED_MESSAGE));
    };
    let producer = handoff.try_claim().map_err(|_| {
        flow_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "application shutdown is in progress",
        )
    })?;

    let epoch = state
        .updater()
        .begin_download(&check.latest_version)
        .map_err(|message| flow_error(StatusCode::CONFLICT, message))?;

    let task_state = state.clone();
    let version = check.latest_version.clone();
    let asset_url = check.action_url.clone();
    producer.spawn(async move {
        run_update_download(
            task_state,
            epoch,
            version,
            asset_url,
            checksum_url,
            asset_name,
        )
        .await;
    });

    Ok(Json(flow_response(state.updater().snapshot())))
}

pub(crate) async fn apply_staged_update(
    state: &AppState,
    handoff: RequestProducerHandoff,
) -> Result<Json<UpdateFlowResponse>, ApiErrorResponse> {
    apply_staged_update_with(state, handoff, |staged_path| {
        self_replace::self_replace(&staged_path)
    })
    .await
}

async fn apply_staged_update_with<Apply, ApplyError>(
    state: &AppState,
    handoff: RequestProducerHandoff,
    apply: Apply,
) -> Result<Json<UpdateFlowResponse>, ApiErrorResponse>
where
    Apply: FnOnce(PathBuf) -> Result<(), ApplyError> + Send + 'static,
    ApplyError: std::fmt::Display + Send + 'static,
{
    let producer = handoff.try_claim().map_err(|_| {
        flow_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "application shutdown is in progress",
        )
    })?;
    let apply_authority = state
        .try_begin_update_apply()
        .map_err(update_apply_admission_error)?;

    let staged_path = state
        .updater()
        .begin_apply()
        .map_err(|message| flow_error(StatusCode::CONFLICT, message))?;

    let task_state = state.clone();
    let settlement = producer.spawn_joinable(async move {
        let replace_result = tokio::task::spawn_blocking(move || apply(staged_path)).await;
        match replace_result {
            Ok(Ok(())) => {
                task_state.updater().mark_restart_pending();
                apply_authority.mark_restart_pending();
                let _ = tokio::fs::remove_dir_all(task_state.updater().staging_dir()).await;
                true
            }
            Ok(Err(error)) => {
                tracing::warn!("failed to apply staged update: {error}");
                task_state.updater().mark_apply_failed(APPLY_FAILED_MESSAGE);
                false
            }
            Err(error) => {
                tracing::warn!("staged update apply task failed: {error}");
                task_state.updater().mark_apply_failed(APPLY_FAILED_MESSAGE);
                false
            }
        }
    });

    match settlement.await {
        Ok(true) => Ok(Json(flow_response(state.updater().snapshot()))),
        Ok(false) => Err(flow_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            APPLY_FAILED_MESSAGE,
        )),
        Err(error) => {
            tracing::warn!("producer-owned update apply task failed: {error}");
            state.updater().mark_apply_failed(APPLY_FAILED_MESSAGE);
            Err(flow_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                APPLY_FAILED_MESSAGE,
            ))
        }
    }
}

fn update_apply_admission_error(error: UpdateApplyAdmissionError) -> ApiErrorResponse {
    let message = match error {
        UpdateApplyAdmissionError::ActiveOperations => UPDATE_BUSY_MESSAGE,
        UpdateApplyAdmissionError::ApplyInProgress => "an update operation is already in progress",
        UpdateApplyAdmissionError::RestartPending => {
            "an update is already applied; restart to finish"
        }
    };
    flow_error(StatusCode::CONFLICT, message)
}

pub(crate) async fn cleanup_update_staging(state: &AppState) {
    let _ = tokio::fs::remove_dir_all(state.updater().staging_dir()).await;
}

async fn run_update_download(
    state: AppState,
    epoch: u64,
    version: String,
    asset_url: String,
    checksum_url: String,
    asset_name: String,
) {
    let staging_dir = state.updater().staging_dir().to_path_buf();
    match download_and_stage(
        &state,
        epoch,
        &version,
        &asset_url,
        &checksum_url,
        &asset_name,
        &staging_dir,
    )
    .await
    {
        Ok(staged_path) => state.updater().mark_ready(epoch, staged_path),
        Err(message) => {
            let _ = tokio::fs::remove_dir_all(&staging_dir).await;
            state.updater().mark_download_failed(epoch, message);
        }
    }
}

async fn download_and_stage(
    state: &AppState,
    epoch: u64,
    version: &str,
    asset_url: &str,
    checksum_url: &str,
    asset_name: &str,
    staging_dir: &Path,
) -> Result<PathBuf, &'static str> {
    let _ = tokio::fs::remove_dir_all(staging_dir).await;
    tokio::fs::create_dir_all(staging_dir)
        .await
        .map_err(|error| {
            tracing::warn!("failed to create update staging dir: {error}");
            STAGING_FAILED_MESSAGE
        })?;

    let expected_hash = fetch_expected_checksum(state.version(), checksum_url, asset_name).await?;

    let archive_path = staging_dir.join(asset_name);
    download_asset_to(state, epoch, asset_url, &archive_path, &expected_hash).await?;

    let staged_path = staging_dir.join(staged_binary_name(version, asset_name));
    let extract_archive = archive_path.clone();
    let extract_staged = staged_path.clone();
    let extract_name = asset_name.to_string();
    tokio::task::spawn_blocking(move || {
        extract_binary(&extract_archive, &extract_staged, &extract_name)
    })
    .await
    .map_err(|error| {
        tracing::warn!("update extraction task failed: {error}");
        ARCHIVE_FAILED_MESSAGE
    })??;

    let _ = tokio::fs::remove_file(&archive_path).await;
    Ok(staged_path)
}

async fn fetch_expected_checksum(
    current_version: &str,
    checksum_url: &str,
    asset_name: &str,
) -> Result<String, &'static str> {
    let response = update_download_client()
        .get(checksum_url)
        .header(USER_AGENT, format!("Axial/{current_version}"))
        .header(ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(|error| {
            tracing::warn!("update checksum fetch failed: {error}");
            DOWNLOAD_FAILED_MESSAGE
        })?;
    if !response.status().is_success() {
        tracing::warn!("update checksum fetch returned HTTP {}", response.status());
        return Err(DOWNLOAD_FAILED_MESSAGE);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_UPDATE_CHECKSUM_BYTES)
    {
        return Err(CHECKSUM_FAILED_MESSAGE);
    }
    let body = response.text().await.map_err(|error| {
        tracing::warn!("update checksum read failed: {error}");
        DOWNLOAD_FAILED_MESSAGE
    })?;
    if body.len() as u64 > MAX_UPDATE_CHECKSUM_BYTES {
        return Err(CHECKSUM_FAILED_MESSAGE);
    }
    parse_checksum_sidecar(&body, asset_name).ok_or(CHECKSUM_FAILED_MESSAGE)
}

fn parse_checksum_sidecar(body: &str, asset_name: &str) -> Option<String> {
    for line in body.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
            continue;
        }
        match parts.next() {
            Some(name) if name.trim_start_matches('*') != asset_name => continue,
            _ => return Some(hash.to_ascii_lowercase()),
        }
    }
    None
}

async fn download_asset_to(
    state: &AppState,
    epoch: u64,
    asset_url: &str,
    archive_path: &Path,
    expected_hash: &str,
) -> Result<(), &'static str> {
    let response = update_download_client()
        .get(asset_url)
        .header(USER_AGENT, format!("Axial/{}", state.version()))
        .header(ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(|error| {
            tracing::warn!("update asset request failed: {error}");
            DOWNLOAD_FAILED_MESSAGE
        })?;
    if !response.status().is_success() {
        tracing::warn!("update asset fetch returned HTTP {}", response.status());
        return Err(DOWNLOAD_FAILED_MESSAGE);
    }
    let total_bytes = response.content_length();
    if total_bytes.is_some_and(|length| length > MAX_UPDATE_ASSET_BYTES) {
        return Err(DOWNLOAD_FAILED_MESSAGE);
    }
    state.updater().set_download_progress(epoch, 0, total_bytes);

    let mut file = tokio::fs::File::create(archive_path)
        .await
        .map_err(|error| {
            tracing::warn!("failed to create update archive file: {error}");
            STAGING_FAILED_MESSAGE
        })?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!("update download stream failed: {error}");
            DOWNLOAD_FAILED_MESSAGE
        })?;
        received += chunk.len() as u64;
        if received > MAX_UPDATE_ASSET_BYTES {
            return Err(DOWNLOAD_FAILED_MESSAGE);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|error| {
            tracing::warn!("failed to write update archive: {error}");
            STAGING_FAILED_MESSAGE
        })?;
        state
            .updater()
            .set_download_progress(epoch, received, total_bytes);
    }
    file.flush().await.map_err(|_| STAGING_FAILED_MESSAGE)?;

    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        tracing::warn!("update archive checksum mismatch");
        return Err(CHECKSUM_FAILED_MESSAGE);
    }
    Ok(())
}

fn staged_binary_name(version: &str, asset_name: &str) -> String {
    if asset_name.ends_with(".zip") {
        format!("axial-staged-{version}.exe")
    } else {
        format!("axial-staged-{version}")
    }
}

fn extract_binary(
    archive_path: &Path,
    staged_path: &Path,
    asset_name: &str,
) -> Result<(), &'static str> {
    if asset_name.ends_with(".tar.gz") {
        extract_tar_gz_binary(archive_path, staged_path, "axial")
    } else if asset_name.ends_with(".zip") {
        extract_zip_binary(archive_path, staged_path, "axial.exe")
    } else {
        Err(ARCHIVE_FAILED_MESSAGE)
    }
}

fn extract_tar_gz_binary(
    archive_path: &Path,
    staged_path: &Path,
    entry_name: &str,
) -> Result<(), &'static str> {
    let archive_file = std::fs::File::open(archive_path).map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive_file));
    let entries = archive.entries().map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
    for entry in entries {
        let entry = entry.map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
        let path = entry.path().map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
        if path != Path::new(entry_name) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(ARCHIVE_FAILED_MESSAGE);
        }
        return write_staged_binary(entry, staged_path);
    }
    Err(ARCHIVE_FAILED_MESSAGE)
}

fn extract_zip_binary(
    archive_path: &Path,
    staged_path: &Path,
    entry_name: &str,
) -> Result<(), &'static str> {
    let archive_file = std::fs::File::open(archive_path).map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
    let mut archive = zip::ZipArchive::new(archive_file).map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
    let entry = archive
        .by_name(entry_name)
        .map_err(|_| ARCHIVE_FAILED_MESSAGE)?;
    if !entry.is_file() {
        return Err(ARCHIVE_FAILED_MESSAGE);
    }
    write_staged_binary(entry, staged_path)
}

fn write_staged_binary(reader: impl Read, staged_path: &Path) -> Result<(), &'static str> {
    let mut bounded = reader.take(MAX_STAGED_BINARY_BYTES + 1);
    let mut staged = std::fs::File::create(staged_path).map_err(|_| STAGING_FAILED_MESSAGE)?;
    let written = std::io::copy(&mut bounded, &mut staged).map_err(|_| STAGING_FAILED_MESSAGE)?;
    if written == 0 || written > MAX_STAGED_BINARY_BYTES {
        return Err(ARCHIVE_FAILED_MESSAGE);
    }
    staged.flush().map_err(|_| STAGING_FAILED_MESSAGE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|_| STAGING_FAILED_MESSAGE)?;
    }
    Ok(())
}

fn asset_file_name(asset_url: &str) -> Option<String> {
    let name = asset_url.rsplit_once('/')?.1;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

fn flow_response(snapshot: UpdateFlowSnapshot) -> UpdateFlowResponse {
    let percent = match (snapshot.phase, snapshot.total_bytes) {
        (
            UpdateFlowPhase::Ready | UpdateFlowPhase::Applying | UpdateFlowPhase::RestartPending,
            _,
        ) => Some(100),
        (UpdateFlowPhase::Downloading, Some(total)) if total > 0 => {
            Some(((snapshot.received_bytes.min(total) * 100) / total) as u8)
        }
        _ => None,
    };
    UpdateFlowResponse {
        phase: flow_phase_id(snapshot.phase),
        version: snapshot.version,
        received_bytes: snapshot.received_bytes,
        total_bytes: snapshot.total_bytes,
        percent,
        message: snapshot.message,
        checked_at: timestamp_utc(),
    }
}

fn flow_phase_id(phase: UpdateFlowPhase) -> &'static str {
    match phase {
        UpdateFlowPhase::Idle => "idle",
        UpdateFlowPhase::Downloading => "downloading",
        UpdateFlowPhase::Ready => "ready",
        UpdateFlowPhase::Applying => "applying",
        UpdateFlowPhase::RestartPending => "restart-pending",
        UpdateFlowPhase::Failed => "failed",
    }
}

fn flow_error(status: StatusCode, message: &str) -> ApiErrorResponse {
    (status, Json(serde_json::json!({ "error": message })))
}

fn update_download_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(UPDATE_DOWNLOAD_CONNECT_TIMEOUT)
                .read_timeout(UPDATE_DOWNLOAD_READ_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore, UpdateOperationAdmissionError};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn checksum_sidecar_parsing_accepts_sha256sum_format() {
        let parsed = parse_checksum_sidecar(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  axial-linux-amd64-1.2.4.tar.gz\n",
            "axial-linux-amd64-1.2.4.tar.gz",
        );
        assert_eq!(
            parsed.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn checksum_sidecar_parsing_accepts_bare_hash_and_binary_marker() {
        let bare = parse_checksum_sidecar(
            "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "axial-windows-amd64-1.2.4.zip",
        );
        assert_eq!(
            bare.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );

        let marked = parse_checksum_sidecar(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *axial-windows-amd64-1.2.4.zip",
            "axial-windows-amd64-1.2.4.zip",
        );
        assert!(marked.is_some());
    }

    #[test]
    fn checksum_sidecar_parsing_rejects_wrong_names_and_bad_hashes() {
        assert!(
            parse_checksum_sidecar(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  other.tar.gz",
                "axial-linux-amd64-1.2.4.tar.gz",
            )
            .is_none()
        );
        assert!(parse_checksum_sidecar("not-a-hash  axial.tar.gz", "axial.tar.gz").is_none());
        assert!(parse_checksum_sidecar("0123456789abcdef", "axial.tar.gz").is_none());
    }

    #[test]
    fn staged_binary_name_matches_platform_archive() {
        assert_eq!(
            staged_binary_name("1.2.4", "axial-linux-amd64-1.2.4.tar.gz"),
            "axial-staged-1.2.4"
        );
        assert_eq!(
            staged_binary_name("1.2.4", "axial-windows-amd64-1.2.4.zip"),
            "axial-staged-1.2.4.exe"
        );
        assert_eq!(
            staged_binary_name("1.2.4", "axial-macos-arm64-1.2.4.tar.gz"),
            "axial-staged-1.2.4"
        );
    }

    #[test]
    fn asset_file_name_takes_url_leaf() {
        assert_eq!(
            asset_file_name(
                "https://github.com/mateoltd/axial/releases/download/v1.2.4/axial-linux-amd64-1.2.4.tar.gz"
            )
            .as_deref(),
            Some("axial-linux-amd64-1.2.4.tar.gz")
        );
        assert!(asset_file_name("no-slash").is_none());
        assert!(asset_file_name("https://example.com/").is_none());
    }

    #[test]
    fn tar_gz_extraction_stages_the_binary() {
        let dir = std::env::temp_dir().join(format!("axial-update-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let archive_path = dir.join("axial-linux-amd64-1.2.4.tar.gz");
        let staged_path = dir.join("axial-staged-1.2.4");

        let payload = b"#!/bin/sh\necho axial\n";
        let archive_file = std::fs::File::create(&archive_path).expect("create archive");
        let encoder = flate2::write::GzEncoder::new(archive_file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "axial", payload.as_slice())
            .expect("append entry");
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");

        extract_binary(
            &archive_path,
            &staged_path,
            "axial-linux-amd64-1.2.4.tar.gz",
        )
        .expect("extract staged binary");
        let staged = std::fs::read(&staged_path).expect("read staged binary");
        assert_eq!(staged, payload);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged_path)
                .expect("staged metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_extraction_stages_the_binary() {
        let dir = std::env::temp_dir().join(format!("axial-update-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let archive_path = dir.join("axial-windows-amd64-1.2.4.zip");
        let staged_path = dir.join("axial-staged-1.2.4.exe");

        let payload = b"MZ fake windows binary";
        let archive_file = std::fs::File::create(&archive_path).expect("create archive");
        let mut writer = zip::ZipWriter::new(archive_file);
        writer
            .start_file("axial.exe", zip::write::SimpleFileOptions::default())
            .expect("start zip entry");
        writer.write_all(payload).expect("write zip entry");
        writer.finish().expect("finish zip");

        extract_binary(&archive_path, &staged_path, "axial-windows-amd64-1.2.4.zip")
            .expect("extract staged binary");
        let staged = std::fs::read(&staged_path).expect("read staged binary");
        assert_eq!(staged, payload);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extraction_rejects_missing_entries_and_unknown_archives() {
        let dir = std::env::temp_dir().join(format!("axial-update-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let archive_path = dir.join("axial-windows-amd64-1.2.4.zip");
        let staged_path = dir.join("staged.exe");

        let archive_file = std::fs::File::create(&archive_path).expect("create archive");
        let mut writer = zip::ZipWriter::new(archive_file);
        writer
            .start_file("other.exe", zip::write::SimpleFileOptions::default())
            .expect("start zip entry");
        writer.write_all(b"payload").expect("write zip entry");
        writer.finish().expect("finish zip");

        assert!(
            extract_binary(&archive_path, &staged_path, "axial-windows-amd64-1.2.4.zip").is_err()
        );
        assert!(extract_binary(&archive_path, &staged_path, "axial-1.2.4.rar").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flow_response_reports_percent_only_with_known_total() {
        let downloading = flow_response(UpdateFlowSnapshot {
            phase: UpdateFlowPhase::Downloading,
            version: "1.2.4".to_string(),
            received_bytes: 25,
            total_bytes: Some(100),
            message: String::new(),
            staged_path: None,
        });
        assert_eq!(downloading.phase, "downloading");
        assert_eq!(downloading.percent, Some(25));

        let unknown_total = flow_response(UpdateFlowSnapshot {
            phase: UpdateFlowPhase::Downloading,
            version: "1.2.4".to_string(),
            received_bytes: 25,
            total_bytes: None,
            message: String::new(),
            staged_path: None,
        });
        assert_eq!(unknown_total.percent, None);

        let ready = flow_response(UpdateFlowSnapshot {
            phase: UpdateFlowPhase::Ready,
            version: "1.2.4".to_string(),
            received_bytes: 100,
            total_bytes: Some(100),
            message: String::new(),
            staged_path: Some(PathBuf::from("/tmp/staged")),
        });
        assert_eq!(ready.phase, "ready");
        assert_eq!(ready.percent, Some(100));
    }

    #[tokio::test]
    async fn cancelled_apply_waiter_retains_exclusive_admission_until_replacement_settles() {
        let root = unique_test_dir("cancelled-apply-waiter");
        let state = test_app_state(&root);
        let epoch = state
            .updater()
            .begin_download("1.2.4")
            .expect("begin test download");
        state
            .updater()
            .mark_ready(epoch, root.join("staged-update"));
        let request = state.try_admit_request().expect("admit apply request");
        let handoff = request.producer_handoff();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let apply_state = state.clone();
        let waiter = tokio::spawn(async move {
            apply_staged_update_with(&apply_state, handoff, move |_| {
                let _ = started_tx.send(());
                release_rx.recv().expect("release replacement");
                Err::<(), _>("forced replacement failure")
            })
            .await
        });
        started_rx.await.expect("replacement started");
        waiter.abort();
        let _ = waiter.await;
        drop(request);

        assert_eq!(
            state.try_admit_update_sensitive_operation().unwrap_err(),
            UpdateOperationAdmissionError::ApplyInProgress
        );

        release_tx.send(()).expect("settle replacement");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.updater().snapshot().phase == UpdateFlowPhase::Failed
                    && let Ok(lease) = state.try_admit_update_sensitive_operation()
                {
                    drop(lease);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement failure settles");
        state
            .try_admit_update_sensitive_operation()
            .expect("failed replacement reopens admission");

        let _ = std::fs::remove_dir_all(root);
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-update-{name}-{nonce}"))
    }
}
