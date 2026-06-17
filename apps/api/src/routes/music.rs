use crate::{
    application::{self, MusicTrackError, MusicTrackRequest},
    state::AppState,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};

#[derive(Debug, serde::Deserialize)]
struct TrackQuery {
    t: Option<usize>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/status", get(handle_music_status))
        .route("/api/v1/music/track", get(handle_music_track))
}

async fn handle_music_status(
    State(state): State<AppState>,
) -> Json<application::MusicStatusResponse> {
    Json(application::music_status(&state).await)
}

async fn handle_music_track(
    State(state): State<AppState>,
    Query(query): Query<TrackQuery>,
) -> impl IntoResponse {
    match application::music_track(&state, MusicTrackRequest { index: query.t }).await {
        Ok(track) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, track.content_type)
            .body(Body::from(track.bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(MusicTrackError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(MusicTrackError::DownloadFailed { body }) => {
            (StatusCode::BAD_GATEWAY, Json(body)).into_response()
        }
    }
}
