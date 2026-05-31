use crate::state::AppState;
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
use std::sync::{Arc, OnceLock};
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
    let tracks = MUSIC_TRACKS
        .into_iter()
        .map(|(file, _)| MusicTrackStatus {
            cached: paths.music_dir.join(file).is_file(),
            file: file.to_string(),
        })
        .collect::<Vec<_>>();

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

    if !local_path.is_file()
        && let Err(error) = download_music_file(&local_path, url).await
    {
        return (
            StatusCode::BAD_GATEWAY,
            Json(music_download_failure_body(&error)),
        )
            .into_response();
    }

    match std::fs::read(&local_path) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn download_music_file(path: &std::path::Path, url: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let bytes = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .bytes()
        .await
        .map_err(|error| error.to_string())?;

    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temp_path, path).map_err(|error| error.to_string())?;
    Ok(())
}

fn music_download_failure_body(_internal_error: &str) -> serde_json::Value {
    serde_json::json!({ "error": MUSIC_DOWNLOAD_FAILURE_COPY })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_error_json_for(internal_error: &str) -> String {
        serde_json::to_string(&music_download_failure_body(internal_error)).unwrap()
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
}
