use crate::application::{self, VersionsResponse};
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, State},
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
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<VersionsResponse>, (StatusCode, Json<serde_json::Value>)> {
    let producer = handoff
        .try_claim()
        .map_err(super::producer_claim_error_response)?;
    application::installed_versions(&state, &producer)
        .await
        .map(Json)
}

async fn handle_version_watch(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let producer = handoff
        .try_claim()
        .map_err(super::producer_claim_error_response)?;
    application::installed_versions(&state, &producer).await?;

    let stream = async_stream::stream! {
        let request_drain = producer.wait_for_request_drain_start();
        tokio::pin!(request_drain);
        let mut ticker = interval(Duration::from_secs(5));
        let mut last_payload = String::new();

        loop {
            tokio::select! {
                biased;
                _ = &mut request_drain => return,
                payload = async {
                    ticker.tick().await;
                    application::installed_versions_event_payload(&state, &producer).await
                } => {
                    if payload != last_payload {
                        last_payload = payload.clone();
                        yield Ok(Event::default().event("versions_changed").data(payload));
                    }
                }
            }
        }
    };

    Ok(Sse::new(stream))
}
