use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

const MUSIC_TRACKS: [(&str, &str); 2] = [
    (
        "vapor-halo.mp3",
        "https://github.com/mateoltd/croopor/releases/download/music-v2/vapor-halo.mp3",
    ),
    (
        "sublunar-hum.mp3",
        "https://github.com/mateoltd/croopor/releases/download/music-v2/sublunar-hum.mp3",
    ),
];

const MUSIC_DOWNLOAD_FAILURE_COPY: &str =
    "Could not load background music. Check your connection and try again.";
const MUSIC_DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MUSIC_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MUSIC_DOWNLOAD_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Serialize)]
struct MusicTrackStatus {
    cached: bool,
    file: String,
}

#[derive(Debug, Serialize)]
struct MusicStatusResponse {
    tracks: Vec<MusicTrackStatus>,
    count: usize,
}

#[derive(Debug, serde::Deserialize)]
struct TrackQuery {
    t: Option<usize>,
}

static MUSIC_DOWNLOAD_LOCKS: OnceLock<Vec<Arc<Mutex<()>>>> = OnceLock::new();

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/status", get(handle_music_status))
        .route("/api/v1/music/track", get(handle_music_track))
}

async fn handle_music_status(State(state): State<AppState>) -> Json<MusicStatusResponse> {
    let paths = state.config().paths();
    let mut tracks = Vec::with_capacity(MUSIC_TRACKS.len());
    for (file, _) in MUSIC_TRACKS {
        tracks.push(MusicTrackStatus {
            cached: is_regular_file(&paths.music_dir.join(file)).await,
            file: file.to_string(),
        });
    }

    Json(MusicStatusResponse {
        count: tracks.len(),
        tracks,
    })
}

async fn handle_music_track(
    State(state): State<AppState>,
    Query(query): Query<TrackQuery>,
) -> impl IntoResponse {
    if MUSIC_TRACKS.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let index = query
        .t
        .unwrap_or(0)
        .min(MUSIC_TRACKS.len().saturating_sub(1));
    let (file, url) = MUSIC_TRACKS[index];
    let paths = state.config().paths();
    let local_path = paths.music_dir.join(file);

    let locks = MUSIC_DOWNLOAD_LOCKS.get_or_init(|| {
        (0..MUSIC_TRACKS.len())
            .map(|_| Arc::new(Mutex::new(())))
            .collect::<Vec<_>>()
    });
    let _guard = locks[index].lock().await;

    if !is_regular_file(&local_path).await
        && let Err(error) = download_music_file(&local_path, url).await
    {
        return (
            StatusCode::BAD_GATEWAY,
            Json(music_download_failure_body(&error)),
        )
            .into_response();
    }

    match async_fs::read(&local_path).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn is_regular_file(path: &Path) -> bool {
    async_fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

async fn download_music_file(path: &Path, url: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }

    let response = music_download_client()
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;

    if response
        .content_length()
        .is_some_and(|length| length > MUSIC_DOWNLOAD_MAX_BYTES)
    {
        return Err("music download is too large".to_string());
    }

    let temp_path = music_temp_path(path);
    let result = write_music_response_to_temp(response, &temp_path).await;
    if result.is_err() {
        let _ = async_fs::remove_file(&temp_path).await;
        return result;
    }

    promote_music_temp(&temp_path, path)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn music_download_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(MUSIC_DOWNLOAD_CONNECT_TIMEOUT)
                .timeout(MUSIC_DOWNLOAD_TIMEOUT)
                .build()
                .unwrap_or_else(|_| Client::new())
        })
        .clone()
}

async fn write_music_response_to_temp(
    response: reqwest::Response,
    temp_path: &Path,
) -> Result<(), String> {
    let mut output = async_fs::File::create(temp_path)
        .await
        .map_err(|error| error.to_string())?;
    let mut total = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        total = total.saturating_add(chunk.len() as u64);
        if total > MUSIC_DOWNLOAD_MAX_BYTES {
            return Err("music download is too large".to_string());
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
    }
    output.flush().await.map_err(|error| error.to_string())
}

async fn promote_music_temp(temp_path: &Path, destination: &Path) -> std::io::Result<()> {
    let first_error = match async_fs::rename(temp_path, destination).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match async_fs::symlink_metadata(temp_path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(first_error),
        Err(error) => return Err(error),
    }

    if let Ok(metadata) = async_fs::symlink_metadata(destination).await {
        let file_type = metadata.file_type();
        if file_type.is_file() || file_type.is_symlink() {
            async_fs::remove_file(destination).await?;
        }
    }

    let result = async_fs::rename(temp_path, destination).await;
    if result.is_err() {
        let _ = async_fs::remove_file(temp_path).await;
    }
    result
}

fn music_temp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("tmp-{}-{nanos:x}", std::process::id()))
}

fn music_download_failure_body(_internal_error: &str) -> serde_json::Value {
    serde_json::json!({ "error": MUSIC_DOWNLOAD_FAILURE_COPY })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn public_error_json_for(internal_error: &str) -> String {
        serde_json::to_string(&music_download_failure_body(internal_error)).unwrap()
    }

    #[tokio::test]
    async fn music_temp_promotion_replaces_existing_file() {
        let root = test_root("music-promote-replace");
        let destination = root.join("track.mp3");
        let temp_path = root.join("track.tmp");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"stale music").expect("write stale file");
        fs::write(&temp_path, b"fresh music").expect("write temp file");

        promote_music_temp(&temp_path, &destination)
            .await
            .expect("promote music temp");

        assert_eq!(
            fs::read(&destination).expect("read promoted file"),
            b"fresh music"
        );
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn music_temp_promotion_preserves_destination_when_temp_is_missing() {
        let root = test_root("music-promote-missing-temp");
        let destination = root.join("track.mp3");
        let temp_path = root.join("missing.tmp");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"existing music").expect("write existing file");

        let error = promote_music_temp(&temp_path, &destination)
            .await
            .expect_err("missing temp should fail");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            fs::read(&destination).expect("read existing file"),
            b"existing music"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn music_temp_promotion_cleans_temp_when_destination_is_directory() {
        let root = test_root("music-promote-directory");
        let destination = root.join("track.mp3");
        let temp_path = root.join("track.tmp");
        fs::create_dir_all(&destination).expect("create destination directory");
        fs::write(&temp_path, b"fresh music").expect("write temp file");

        let result = promote_music_temp(&temp_path, &destination).await;

        assert!(result.is_err());
        assert!(destination.is_dir());
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn music_download_failure_uses_bounded_public_copy() {
        let body = music_download_failure_body("request timed out");

        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some(MUSIC_DOWNLOAD_FAILURE_COPY)
        );
    }

    #[test]
    fn music_download_failure_does_not_expose_url_or_request_fragments() {
        let internal_error = concat!(
            "error sending request for url ",
            "https://github.com/mateoltd/croopor/releases/download/music-v2/vapor-halo.mp3"
        );
        let public_json = public_error_json_for(internal_error);

        assert!(!public_json.contains("error sending request"));
        assert!(!public_json.contains("https://github.com"));
        assert!(!public_json.contains("vapor-halo.mp3"));
    }

    #[test]
    fn music_download_failure_does_not_expose_unix_paths() {
        let public_json = public_error_json_for(
            "failed to rename /home/zero/.local/share/croopor/music/vapor-halo.tmp",
        );

        assert!(!public_json.contains("/home/zero"));
        assert!(!public_json.contains(".local/share/croopor"));
        assert!(!public_json.contains("vapor-halo.tmp"));
    }

    #[test]
    fn music_download_failure_does_not_expose_windows_paths() {
        let public_json = public_error_json_for(
            r"failed to write C:\Users\Zero\AppData\Roaming\Croopor\music\vapor-halo.tmp",
        );

        assert!(!public_json.contains(r"C:\Users"));
        assert!(!public_json.contains("AppData"));
        assert!(!public_json.contains("Roaming"));
    }

    #[test]
    fn music_download_failure_does_not_expose_raw_os_text() {
        let public_json = public_error_json_for("Permission denied (os error 13)");

        assert!(!public_json.contains("Permission denied"));
        assert!(!public_json.contains("os error 13"));
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "croopor-music-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }
}
