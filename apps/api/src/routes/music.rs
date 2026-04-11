use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

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

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/music/status", get(handle_music_status))
}

async fn handle_music_status(State(state): State<AppState>) -> Json<MusicStatusResponse> {
    let paths = state.config().paths();
    let tracks = ["vapor-halo.mp3", "sublunar-hum.mp3"]
        .into_iter()
        .map(|file| MusicTrackStatus {
            cached: paths.music_dir.join(file).is_file(),
            file: file.to_string(),
        })
        .collect::<Vec<_>>();

    Json(MusicStatusResponse {
        count: tracks.len(),
        tracks,
    })
}
