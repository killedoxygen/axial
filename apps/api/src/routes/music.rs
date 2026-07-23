use crate::{
    application::{self, MusicTrackError, MusicTrackRequest},
    routes::producer_claim_error_response,
    state::{AppState, RequestProducerHandoff},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Query, State},
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
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Response<Body> {
    match application::music_status(&state, handoff).await {
        Ok(status) => Json(status).into_response(),
        Err(application::MusicStatusUnavailable) => {
            producer_claim_error_response(crate::state::LifecycleAdmissionError).into_response()
        }
    }
}

async fn handle_music_track(
    State(state): State<AppState>,
    Query(query): Query<TrackQuery>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> impl IntoResponse {
    match application::music_track(&state, MusicTrackRequest { index: query.t }, handoff).await {
        Ok(track) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, track.content_type)
            .body(Body::from(track.bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(error) => music_track_error_response(error),
    }
}

fn music_track_error_response(error: MusicTrackError) -> Response<Body> {
    match error {
        MusicTrackError::NotFound => StatusCode::NOT_FOUND.into_response(),
        MusicTrackError::Unavailable => {
            producer_claim_error_response(crate::state::LifecycleAdmissionError).into_response()
        }
        MusicTrackError::DownloadFailed { body } => {
            (StatusCode::BAD_GATEWAY, Json(body)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_errors_preserve_route_status_semantics() {
        assert_eq!(
            music_track_error_response(MusicTrackError::Unavailable).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            music_track_error_response(MusicTrackError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            music_track_error_response(MusicTrackError::DownloadFailed {
                body: serde_json::json!({ "error": "bounded" }),
            })
            .status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
