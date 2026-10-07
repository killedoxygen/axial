use axial_app::{
    install::{
        model::{
            InstallEvent, InstallQueueRequest, InstallQueueStateResponse, InstallStatusResponse,
        },
        queue::{InstallError, InstallQueue},
    },
    instances::setup::SetupService,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{delete, get, post},
};
use futures_util::{Stream, stream};
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc};

type ApiError = (StatusCode, Json<Value>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryQuery {
    expected_install_id: Option<String>,
}

/// Mount beneath the composition owner's capability and stream-ticket checks.
pub fn router(queue: Arc<InstallQueue>, setup: Arc<SetupService>) -> Router {
    Router::new()
        .route("/api/v1/install/queue", get(snapshot).post(enqueue))
        .route("/api/v1/install/queue/events", get(queue_events))
        .route("/api/v1/install/queue/{id}", delete(remove))
        .route("/api/v1/install/{id}/status", get(status))
        .route("/api/v1/install/{id}/events", get(install_events))
        .route("/api/v1/install/{id}/cancel", post(cancel))
        .route("/api/v1/loaders/install/{id}/events", get(install_events))
        .with_state(queue.clone())
        .merge(
            Router::new()
                .route("/api/v1/install/queue/retry", post(retry))
                .with_state((queue, setup)),
        )
        .layer(DefaultBodyLimit::max(64 << 10))
}

async fn snapshot(State(queue): State<Arc<InstallQueue>>) -> Json<InstallQueueStateResponse> {
    Json(queue.snapshot())
}
async fn enqueue(
    State(queue): State<Arc<InstallQueue>>,
    request: Result<Json<InstallQueueRequest>, JsonRejection>,
) -> Result<Json<InstallQueueStateResponse>, ApiError> {
    let Json(request) = request.map_err(|_| failure(InstallError::InvalidRequest))?;
    queue.enqueue(request).await.map(Json).map_err(failure)
}
async fn retry(
    State((queue, setup)): State<(Arc<InstallQueue>, Arc<SetupService>)>,
    query: Result<Query<RetryQuery>, QueryRejection>,
    request: Result<Json<InstallQueueRequest>, JsonRejection>,
) -> Result<Json<InstallQueueStateResponse>, ApiError> {
    let Query(query) = query.map_err(|_| failure(InstallError::InvalidRequest))?;
    let Json(request) = request.map_err(|_| failure(InstallError::InvalidRequest))?;
    if let Some(id) = query.expected_install_id {
        if !uuid::Uuid::parse_str(&id).is_ok_and(|parsed| parsed.to_string() == id) {
            return Err(failure(InstallError::InvalidRequest));
        }
        return queue
            .retry_retained(&id, &request)
            .await
            .map(Json)
            .map_err(failure);
    }
    if let Some(response) = setup
        .retry_queued_setup(&request)
        .await
        .map_err(super::instances::error)?
    {
        return Ok(Json(response));
    }
    queue.retry(request).await.map(Json).map_err(failure)
}
async fn remove(
    State(queue): State<Arc<InstallQueue>>,
    Path(id): Path<String>,
) -> Result<Json<InstallQueueStateResponse>, ApiError> {
    queue.remove(&id).await.map(Json).map_err(failure)
}
async fn status(
    State(queue): State<Arc<InstallQueue>>,
    Path(id): Path<String>,
) -> Result<Json<InstallStatusResponse>, ApiError> {
    queue.status(&id).map(Json).map_err(failure)
}
async fn cancel(
    State(queue): State<Arc<InstallQueue>>,
    Path(id): Path<String>,
) -> Result<Json<InstallStatusResponse>, ApiError> {
    queue.cancel(&id).map(Json).map_err(failure)
}

async fn queue_events(
    State(queue): State<Arc<InstallQueue>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (snapshot, receiver) = queue.subscribe();
    Sse::new(stream::unfold(
        (Some(snapshot), receiver, queue),
        |(initial, mut receiver, queue)| async move {
            if queue.events_closed() {
                return None;
            }
            let value = match initial {
                Some(value) => value,
                None => {
                    receiver.changed().await.ok()?;
                    receiver.borrow_and_update().clone()
                }
            };
            if queue.events_closed() {
                return None;
            }
            let event = Event::default()
                .id(value.revision.to_string())
                .json_data(InstallEvent {
                    revision: value.revision,
                    value,
                })
                .ok()?;
            Some((Ok(event), (None, receiver, queue)))
        },
    ))
    .keep_alive(KeepAlive::default())
}
async fn install_events(
    State(queue): State<Arc<InstallQueue>>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Subscribe before reading status so the current snapshot and later events
    // cannot have a gap. Slow clients rebase to the queue's latest projection.
    let (_, receiver) = queue.subscribe();
    let initial = queue.status(&id).map_err(failure)?;
    Ok(Sse::new(stream::unfold(
        (Some(initial), receiver, queue, id, false),
        |(initial, mut receiver, queue, id, finished)| async move {
            if finished || queue.events_closed() {
                return None;
            }
            let value = match initial {
                Some(value) => value,
                None => {
                    receiver.changed().await.ok()?;
                    queue.status(&id).ok()?
                }
            };
            let finished = value.done;
            let event = Event::default()
                .id(value.revision.to_string())
                .json_data(InstallEvent {
                    revision: value.revision,
                    value,
                })
                .ok()?;
            Some((Ok(event), (None, receiver, queue, id, finished)))
        },
    ))
    .keep_alive(KeepAlive::default()))
}
pub(crate) fn failure(error: InstallError) -> ApiError {
    let status = match error {
        InstallError::InvalidRequest => StatusCode::BAD_REQUEST,
        InstallError::NotFound => StatusCode::NOT_FOUND,
        InstallError::Busy
        | InstallError::SettlementRequired
        | InstallError::NotReady
        | InstallError::ClientJarMissing
        | InstallError::ClientJarCorrupt
        | InstallError::VersionJsonMissing
        | InstallError::LibrariesMissing
        | InstallError::LibrariesCorrupt => StatusCode::CONFLICT,
        InstallError::AtCapacity => StatusCode::TOO_MANY_REQUESTS,
        InstallError::ContentUnavailable
        | InstallError::LoaderUnavailable
        | InstallError::Closed => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"error":error.to_string()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        instances::model::{Instance, InstanceError, InstanceId},
        settings::InstanceSettings,
        storage::rusqlite::params,
    };
    use std::time::Duration;
    use tokio::{net::TcpListener, sync::Semaphore};

    async fn post(
        services: &crate::DesktopServices,
        path: &str,
        request: Value,
    ) -> (StatusCode, Value) {
        let bootstrap = services.server.bootstrap();
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .post(format!("{}{path}", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, bootstrap.capability)
            .json(&request)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    #[tokio::test]
    async fn retained_retry_selector_refuses_stale_or_malformed_identity_without_enqueuing() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let services = crate::start_in_profile(root.path().join("profile"), None)
            .await
            .unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        for (query, expected) in [
            (
                "expected_install_id=../path".to_owned(),
                StatusCode::BAD_REQUEST,
            ),
            ("expected_install_id=".to_owned(), StatusCode::BAD_REQUEST),
            (
                format!("expected_install_id={id}&expected_install_id={id}"),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("expected_install_id={id}&unknown=true"),
                StatusCode::BAD_REQUEST,
            ),
            (format!("expected_install_id={id}"), StatusCode::NOT_FOUND),
        ] {
            let (status, _) = post(
                &services,
                &format!("/api/v1/install/queue/retry?{query}"),
                json!({"kind":"vanilla","version_id":"must-not-enqueue"}),
            )
            .await;
            assert_eq!(status, expected, "{query}");
            let snapshot = services.installs.snapshot();
            assert!(snapshot.active.is_none());
            assert!(snapshot.items.is_empty());
        }
        services.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn composed_retry_route_prioritizes_an_ordinary_intent() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requested = Arc::new(Semaphore::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let provider = tokio::spawn({
            let requested = requested.clone();
            let gate = gate.clone();
            async move {
                axum::serve(
                    listener,
                    Router::new().fallback(move || {
                        let requested = requested.clone();
                        let gate = gate.clone();
                        async move {
                            requested.add_permits(1);
                            gate.acquire().await.unwrap().forget();
                            StatusCode::BAD_GATEWAY
                        }
                    }),
                )
                .await
                .unwrap();
            }
        });
        let services = crate::start_profile_with_test_endpoints(
            root.path().join("profile"),
            axial_minecraft::download::InstallTestEndpoints::from_loopback_base_url(&base).unwrap(),
        )
        .await
        .unwrap();
        let (status, response) = post(
            &services,
            "/api/v1/install/queue",
            json!({
                "kind":"vanilla", "version_id":"blocked"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        tokio::time::timeout(Duration::from_secs(5), requested.acquire())
            .await
            .expect("accepted installer reaches loopback provider")
            .unwrap()
            .forget();
        let (status, response) = post(
            &services,
            "/api/v1/install/queue",
            json!({
                "kind":"vanilla", "version_id":"pending"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let (status, response) = post(
            &services,
            "/api/v1/install/queue/retry",
            json!({
                "kind":"vanilla", "version_id":"retry"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["items"][0]["install_item"]["version_id"], "retry");
        assert_eq!(
            response["items"][1]["install_item"]["version_id"],
            "pending"
        );
        for item in response["items"].as_array().unwrap() {
            services
                .installs
                .remove(item["queue_id"].as_str().unwrap())
                .await
                .unwrap();
        }
        gate.add_permits(32);
        services.server.shutdown().await.unwrap();
        provider.abort();
    }

    #[tokio::test]
    async fn composed_retry_route_rejects_a_changed_pending_setup_before_queue_admission() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let services = crate::start_in_profile(root.path().join("profile"), None)
            .await
            .unwrap();
        let id = InstanceId::new();
        let library_id = services.library.admit().unwrap().library_id().to_string();
        let registry = services.instances.registry();
        let stored = json!({
            "selection_id":"vanilla|1.21.4", "version_id":"1.21.4",
            "target":{"loader":"vanilla", "game_version":"1.21.4", "supports_mods":false},
            "selections":[{"canonical_id":"modrinth:accepted", "kind":"resource_pack", "version_id":"v1"}],
            "install":{"kind":"vanilla", "version_id":"1.21.4"},
            "fingerprint":"fixture", "expires_at_ms":0, "create":null
        });
        registry.storage().transaction(|tx| {
            let reserved = registry.reserve(tx, Instance {
                id: id.clone(), name:"Pending fixture".into(), version_id:"1.21.4".into(),
                created_at:"2026-01-01T00:00:00Z".into(), last_played_at:String::new(),
                art_seed:0, settings:InstanceSettings::default(), icon:String::new(),
                accent:String::new(), loader_key:"vanilla".into(), minecraft_version:"1.21.4".into(), revision:1,
            }, &library_id)?;
            // This rejection fixture intentionally has no filesystem authority.
            // Changed retry intent must fail before directory admission.
            registry.commit_reserved(tx, &reserved, "unadmitted-retry-fixture")?;
            tx.execute("INSERT INTO instance_setups(instance_id,plan_id,request_json,phase) VALUES(?1,?2,?3,'pending')", params![
                id.as_str(), uuid::Uuid::new_v4().to_string(), stored.to_string(),
            ])?;
            Ok::<_, InstanceError>(())
        }).unwrap();
        let before = services.installs.snapshot();
        let (status, response) = post(
            &services,
            "/api/v1/install/queue/retry",
            json!({
                "kind":"content", "instance_id":id, "label":"Setting up Pending fixture",
                "action":{"kind":"install", "selections":[{
                    "canonical_id":"modrinth:changed", "kind":"resource_pack", "version_id":"v2"
                }], "allow_incompatible":false}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{response}");
        assert_eq!(
            response,
            json!({"error":InstanceError::Conflict.to_string()})
        );
        assert_eq!(services.installs.snapshot(), before);
        assert!(
            !services
                .profile_root
                .join("instances")
                .join(id.as_str())
                .exists()
        );
        services.server.shutdown().await.unwrap();
    }
}
