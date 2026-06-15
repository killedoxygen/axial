use crate::application::{self, VersionsResponse};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::get,
};
use std::{convert::Infallible, time::Duration};
use tokio::time::interval;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/versions", get(handle_versions))
        .route("/api/v1/versions/watch", get(handle_version_watch))
}

async fn handle_versions(
    State(state): State<AppState>,
) -> Result<Json<VersionsResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::installed_versions(&state).await.map(Json)
}

async fn handle_version_watch(
    State(state): State<AppState>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    application::installed_versions(&state).await?;

    let stream = async_stream::stream! {
        let mut ticker = interval(Duration::from_secs(5));
        let mut last_payload = String::new();

        loop {
            ticker.tick().await;
            let payload = application::installed_versions_event_payload(&state).await;
            if payload != last_payload {
                last_payload = payload.clone();
                yield Ok(Event::default().event("versions_changed").data(payload));
            }
        }
    };

    Ok(Sse::new(stream))
}
