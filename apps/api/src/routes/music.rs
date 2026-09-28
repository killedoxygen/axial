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
use serde_json::{Value, json};
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

async fn status(
    State(music): State<Arc<MusicService>>,
) -> Result<Json<MusicStatusResponse>, (StatusCode, Json<Value>)> {
    music.status().await.map(Json).map_err(error_response)
}

async fn track(
    State(music): State<Arc<MusicService>>,
    query: Result<Query<TrackQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid music track request."})),
        )
            .into_response();
    };
    match music.track(query.t).await {
        Ok(track) => (
            [(header::CONTENT_TYPE, track.content_type)],
            Body::from(Bytes::from_owner(track.bytes)),
        )
            .into_response(),
        Err(MusicError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => error_response(error).into_response(),
    }
}

fn error_response(error: MusicError) -> (StatusCode, Json<Value>) {
    let status = match error {
        MusicError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        MusicError::NotFound => StatusCode::NOT_FOUND,
        MusicError::DownloadFailed => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({"error": error.to_string()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        library::{LibraryLifecycle, LibraryOpenOutcome},
        tasks::TaskOwner,
    };
    use std::time::Duration;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn actual_http_serves_cached_inventory_audio_and_fixed_query_errors() {
        let directory =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = match LibraryLifecycle::open(directory.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated music fixture failed: {other:?}"),
        };
        let tasks = TaskOwner::new(8).unwrap();
        let music = Arc::new(MusicService::new(library.clone(), tasks.clone()).unwrap());
        std::fs::create_dir(directory.path().join("music")).unwrap();
        std::fs::write(
            directory.path().join("music/vapor-halo.mp3"),
            b"first-music",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("music/sublunar-hum.mp3"),
            b"second-music",
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(music.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let status: Value = client
            .get(format!("{base}/api/v1/music/status"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            status,
            json!({"tracks": [{"cached": true, "file": "vapor-halo.mp3"}, {"cached": true, "file": "sublunar-hum.mp3"}], "count": 2})
        );
        for (suffix, expected) in [
            ("", b"first-music".as_slice()),
            ("?t=9999", b"second-music".as_slice()),
        ] {
            let response = client
                .get(format!("{base}/api/v1/music/track{suffix}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mpeg");
            assert_eq!(response.bytes().await.unwrap().as_ref(), expected);
        }
        for suffix in ["?t=-1", "?t=private", "?path=/Users/private", "?t=0&t=1"] {
            let response = client
                .get(format!("{base}/api/v1/music/track{suffix}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({"error": "Invalid music track request."})
            );
        }
        tasks.shutdown(Duration::from_secs(5)).await.unwrap();
        music.settle().unwrap();
        assert_eq!(
            client
                .get(format!("{base}/api/v1/music/status"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            client
                .get(format!("{base}/api/v1/music/track"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn domain_failures_retain_route_status_and_opaque_copy() {
        assert_eq!(
            error_response(MusicError::Unavailable).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            error_response(MusicError::NotFound).0,
            StatusCode::NOT_FOUND
        );
        let (status, Json(body)) = error_response(MusicError::DownloadFailed);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body,
            json!({"error": "Could not load background music. Check your connection and try again."})
        );
    }
}
