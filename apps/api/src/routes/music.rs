use super::error::{ApiError, ApiResult, Extracted};
use axial_app::music::{MusicError, MusicService, MusicStatusResponse};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Query, State, rejection::QueryRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackQuery {
    t: Option<usize>,
}

pub fn router(music: Arc<MusicService>) -> Router {
    Router::new()
        .route("/api/v1/music/status", get(status))
        .route("/api/v1/music/track", get(track))
        .with_state(music)
}

async fn status(State(music): State<Arc<MusicService>>) -> ApiResult<MusicStatusResponse> {
    Ok(Json(music.status().await?))
}

async fn track(
    State(music): State<Arc<MusicService>>,
    query: Result<Query<TrackQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let query = query
        .or_refuse(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid music track request."))?;
    match music.track(query.t).await {
        Ok(track) => Ok((
            [(header::CONTENT_TYPE, track.content_type)],
            Body::from(Bytes::from_owner(track.bytes)),
        )
            .into_response()),
        Err(MusicError::NotFound) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use crate::fixture::Profile;
    use reqwest::{Method, StatusCode};
    use serde_json::json;

    #[tokio::test]
    async fn serves_cached_inventory_audio_and_fixed_query_errors() {
        let profile = Profile::start().await;
        let music = profile.services.profile_root.join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("vapor-halo.mp3"), b"first-music").unwrap();
        std::fs::write(music.join("sublunar-hum.mp3"), b"second-music").unwrap();
        assert_eq!(
            profile.api.get("/api/v1/music/status").await,
            json!({"tracks": [{"cached": true, "file": "vapor-halo.mp3"}, {"cached": true, "file": "sublunar-hum.mp3"}], "count": 2})
        );
        for (suffix, expected) in [
            ("", b"first-music".as_slice()),
            ("?t=9999", b"second-music"),
        ] {
            let path = format!("/api/v1/music/track{suffix}");
            let response = profile
                .api
                .builder(Method::GET, &path)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], "audio/mpeg");
            assert_eq!(response.bytes().await.unwrap().as_ref(), expected);
        }
        for suffix in ["?t=-1", "?t=private", "?path=/Users/private", "?t=0&t=1"] {
            assert_eq!(
                profile
                    .api
                    .read(&format!("/api/v1/music/track{suffix}"))
                    .await,
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "Invalid music track request."})
                )
            );
        }
        profile.close().await;
    }
}
