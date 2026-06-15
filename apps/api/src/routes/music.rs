use crate::{
    execution::download::{DownloadToTempRequest, download_url_to_temp},
    state::{
        AppState,
        ownership::{CurrentArtifact, classify_current_artifact},
    },
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use reqwest::Client;
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::fs as async_fs;
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
        && let Err(error) = download_music_file(&local_path, file, url).await
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

async fn download_music_file(path: &Path, file: &str, url: &str) -> Result<(), String> {
    let target = classify_current_artifact(CurrentArtifact::MusicCacheFile, file).target;
    let client = music_download_client();
    download_url_to_temp(
        DownloadToTempRequest::new(target, path, url).with_max_bytes(MUSIC_DOWNLOAD_MAX_BYTES),
        &client,
    )
    .await
    .map(|_| ())
    .map_err(|error| error.to_string())
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

fn music_download_failure_body(_internal_error: &str) -> serde_json::Value {
    serde_json::json!({ "error": MUSIC_DOWNLOAD_FAILURE_COPY })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn public_error_json_for(internal_error: &str) -> String {
        serde_json::to_string(&music_download_failure_body(internal_error)).unwrap()
    }

    #[tokio::test]
    async fn music_download_file_caches_track_through_execution_capability() {
        let root = test_root("download-cache");
        let destination = root.join("track.mp3");
        let (url, server) = spawn_music_server(b"fresh music".to_vec()).await;

        download_music_file(&destination, "track.mp3", &url)
            .await
            .expect("download music file");
        server.await.expect("music server task");

        assert_eq!(
            fs::read(&destination).expect("read cached track"),
            b"fresh music"
        );
        assert_no_temp_files(&root, &destination);
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

    async fn spawn_music_server(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind music test server");
        let addr = listener.local_addr().expect("music test server addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept music request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await.expect("read music request");
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write music response headers");
            stream
                .write_all(&body)
                .await
                .expect("write music response body");
        });
        (format!("http://{addr}/track.mp3"), server)
    }

    fn assert_no_temp_files(root: &Path, destination: &Path) {
        let entries = fs::read_dir(root)
            .expect("read music root")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect music root entries");
        let leftovers = entries
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| path != destination)
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
    }
}
