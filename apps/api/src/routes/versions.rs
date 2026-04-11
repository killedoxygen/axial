use crate::state::AppState;
use axum::{
    Json, Router,
    extract::State,
    response::sse::{Event, Sse},
    routing::get,
};
use croopor_minecraft::{VersionEntry, scan_versions};
use serde::Serialize;
use std::{convert::Infallible, path::PathBuf, time::Duration};
use tokio::time::interval;

#[derive(Debug, Serialize)]
struct VersionsResponse {
    versions: Vec<VersionEntry>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/versions", get(handle_versions))
        .route("/api/v1/versions/watch", get(handle_version_watch))
}

async fn handle_versions(
    State(state): State<AppState>,
) -> Result<Json<VersionsResponse>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    let Some(mc_dir) = state.mc_dir() else {
        return Err((
            axum::http::StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "minecraft directory not configured" })),
        ));
    };

    let versions = scan_versions(&PathBuf::from(mc_dir)).map_err(|error| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to scan versions: {error}") })),
        )
    })?;

    Ok(Json(VersionsResponse { versions }))
}

async fn handle_version_watch(
    State(state): State<AppState>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (axum::http::StatusCode, Json<serde_json::Value>),
> {
    let Some(mc_dir) = state.mc_dir() else {
        return Err((
            axum::http::StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "minecraft directory not configured" })),
        ));
    };

    let mc_dir = PathBuf::from(mc_dir);
    let stream = async_stream::stream! {
        let mut ticker = interval(Duration::from_secs(5));
        let mut last_payload = String::new();

        loop {
            ticker.tick().await;
            let versions = scan_versions(&mc_dir).unwrap_or_default();
            let payload = serde_json::to_string(&serde_json::json!({ "versions": versions })).unwrap_or_else(|_| "{\"versions\":[]}".to_string());
            if payload != last_payload {
                last_payload = payload.clone();
                yield Ok(Event::default().event("versions_changed").data(payload));
            }
        }
    };

    Ok(Sse::new(stream))
}
