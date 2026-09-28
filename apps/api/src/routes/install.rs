use axial_app::{
    install::{
        history::HistoryPage,
        model::{
            InstallEvent, InstallQueueRequest, InstallQueueStateResponse, InstallStatusResponse,
        },
        queue::{InstallError, InstallQueue},
    },
    instances::{create::InstanceService, model::InstanceId, setup::SetupService},
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    instance_id: InstanceId,
    after: Option<String>,
}

/// Mount beneath the composition owner's capability and stream-ticket checks.
pub fn router(
    queue: Arc<InstallQueue>,
    setup: Arc<SetupService>,
    instances: Arc<InstanceService>,
) -> Router {
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
        .merge(
            Router::new()
                .route("/api/v1/install/history", get(history))
                .with_state(instances),
        )
        .layer(DefaultBodyLimit::max(64 << 10))
}

async fn history(
    State(instances): State<Arc<InstanceService>>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<HistoryPage>, ApiError> {
    let Query(query) = query.map_err(|_| failure(InstallError::InvalidRequest))?;
    tokio::task::spawn_blocking(move || {
        instances.imported_install_history(&query.instance_id, query.after.as_deref())
    })
    .await
    .map_err(|_| failure(InstallError::Storage))?
    .map(Json)
    .map_err(super::instances::error)
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
        InstallError::Busy | InstallError::SettlementRequired | InstallError::NotReady => {
            StatusCode::CONFLICT
        }
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
        import::{Inventory, ReadOnlySource},
        instances::duplicate::DuplicateRequest,
        instances::model::{Instance, InstanceError, InstanceId},
        settings::InstanceSettings,
        storage::{StorageError, rusqlite::params},
    };
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };
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

    async fn get(services: &crate::DesktopServices, path: &str) -> (StatusCode, Value) {
        let bootstrap = services.server.bootstrap();
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("{}{path}", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, bootstrap.capability)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    fn source_snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
        fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, (Vec<u8>, SystemTime)>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &entry.path(), result);
                } else {
                    result.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        (
                            fs::read(entry.path()).unwrap(),
                            entry.metadata().unwrap().modified().unwrap(),
                        ),
                    );
                }
            }
        }
        let mut result = BTreeMap::new();
        visit(root, root, &mut result);
        result
    }

    fn assert_no_install_authority(services: &crate::DesktopServices) {
        let queue = services.installs.snapshot();
        assert!(queue.active.is_none());
        assert!(queue.items.is_empty());
        assert!(services.sessions.snapshots().is_empty());
        services
            .settings
            .metadata()
            .read::<_, StorageError>(|connection| {
                let counts: (u64, u64, u64) = connection.query_row(
                    "SELECT (SELECT COUNT(*) FROM install_queue),
                 (SELECT COUNT(*) FROM installed_versions), (SELECT COUNT(*) FROM launch_intents)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                assert_eq!(counts, (0, 0, 0));
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn imported_content_history_http_preserves_lossless_read_only_evidence_after_reopen() {
        const LEGACY: &str = "0000000000000001";
        const SEQUENCE: u64 = 9_007_199_254_740_993;
        const METRICS: [&str; 13] = [
            "checksum_mismatch",
            "metadata_invalid",
            "metadata_missing",
            "interrupted",
            "network_failure",
            "permission_failure",
            "promote_failed",
            "provider_failure",
            "size_mismatch",
            "temp_discarded",
            "temp_write_failed",
            "written_to_temp",
            "promoted",
        ];
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let baseline = root.path().join("baseline");
        fs::create_dir(&baseline).unwrap();
        super::super::import::tests::copy_fixture(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../acceptance/fixtures/profiles/offline-vanilla"),
            &baseline,
        );
        let target = |kind: &str, id: &str| {
            json!({
                "system":"Application","kind":kind,"id":id,"ownership":"LauncherManaged"
            })
        };
        let step = |id: &str, phase: &str, result: &str, facts: Value| {
            json!({
                "step_id":id,"phase":phase,"result":result,"changed_target":null,
                "generated_facts":facts,"rollback":"NotApplicable","guardian_fact_ids":[],"metrics":null
            })
        };
        let mut terminal = step(
            "content_progress_done",
            "Downloading",
            "Completed",
            json!(["install_phase:done", "install_done:true"]),
        );
        terminal["metrics"] = json!({"kind":"content_download","values":
            METRICS.into_iter().map(|key| (key.to_owned(), json!(u64::MAX))).collect::<serde_json::Map<_, _>>()});
        let operation_id = "op-00000000-0000-4000-8000-000000000001";
        let operation = json!({
            "journal_id":format!("journal-{operation_id}"),"operation_id":operation_id,
            "sequence":SEQUENCE,"parent_operation_id":null,"command":"ModifyInstanceContent",
            "intent":{"kind":"generic"},"status":"Succeeded","owner":"Application","ownership":"LauncherManaged",
            "targets":[target("Session", "content-00000000000000000000000000000001"),target("Instance",LEGACY)],
            "planned_steps":[step("modify_instance_content","Planning","Planned",json!([]))],
            "completed_steps":[terminal],"failure_point":null,"rollback":"NotApplicable",
            "guardian_diagnosis_ids":[],"outcome":"Succeeded","reconciliation_attempt":null,
            "reconciliation_terminal":null,"persisted_state_repair_attempt":null,
            "persisted_state_repair_terminal":null,"guardian_install_terminal":null
        });
        let cancelled_id = "op-00000000-0000-4000-8000-000000000002";
        let mut cancelled = operation.clone();
        cancelled["journal_id"] = json!(format!("journal-{cancelled_id}"));
        cancelled["operation_id"] = json!(cancelled_id);
        cancelled["sequence"] = json!(SEQUENCE + 1);
        cancelled["targets"][0] = target("Session", "content-00000000000000000000000000000002");
        cancelled["status"] = json!("Failed");
        cancelled["outcome"] = json!("Failed");
        cancelled["failure_point"] = json!("content_initialization_cancelled");
        cancelled["completed_steps"] = json!([step(
            "content_progress_initializing",
            "Failed",
            "Failed",
            json!([
                "install_phase:initializing",
                "install_done:true",
                "install_error:true"
            ])
        )]);
        fs::create_dir_all(baseline.join("state")).unwrap();
        fs::write(baseline.join("state/operation-journals.json"), serde_json::to_vec(&json!({
            "schema":"axial.state.operation_journals.v10","next_sequence":SEQUENCE+2,"entries":[operation,cancelled]
        })).unwrap()).unwrap();
        let unchanged = source_snapshot(&baseline);
        let profile = root.path().join("replacement");
        let services = crate::start_in_profile(profile.clone(), None)
            .await
            .unwrap();
        let source = ReadOnlySource::from_native_selection(
            services.library.admit_application_root().unwrap(),
            &baseline,
        )
        .unwrap();
        let inventory = Inventory::capture(&source, &BTreeMap::new()).unwrap();
        let preview = services.imports.admit(inventory).unwrap();
        assert!(
            preview
                .instances
                .iter()
                .find(|instance| instance.legacy_id == LEGACY)
                .unwrap()
                .ordinary_import_available
        );
        let bootstrap = services.server.bootstrap();
        let unauthorized = reqwest::Client::new()
            .get(format!("{}/api/v1/install/history", bootstrap.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        for query in [
            "".to_owned(), "instance_id=not-a-uuid".to_owned(),
            "instance_id=00000000-0000-0000-0000-000000000000".to_owned(),
            "instance_id=12345678-1234-4234-8234-123456789abc&unknown=true".to_owned(),
            "instance_id=12345678-1234-4234-8234-123456789abc&instance_id=12345678-1234-4234-8234-123456789abc".to_owned(),
        ] {
            assert_eq!(get(&services, &format!("/api/v1/install/history?{query}")).await.0,
                StatusCode::BAD_REQUEST, "{query}");
        }
        assert_eq!(
            get(
                &services,
                "/api/v1/install/history?instance_id=12345678-1234-4234-8234-123456789abc"
            )
            .await,
            (
                StatusCode::NOT_FOUND,
                json!({"error":InstanceError::NotFound.to_string()})
            )
        );
        let import_request = json!({"fingerprint":preview.fingerprint,"legacy_id":LEGACY});
        let (status, imported) = post(
            &services,
            "/api/v1/import/instances",
            import_request.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        let id = imported["instance"]["id"].as_str().unwrap();
        let path = format!("/api/v1/install/history?instance_id={id}");
        let (status, page) = get(&services, &path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(page["next_after"].is_null());
        let records = page["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        let record = records
            .iter()
            .find(|record| record["operation_id"] == operation_id)
            .unwrap();
        let history_id = record["id"].as_str().unwrap();
        assert_eq!(
            history_id.strip_prefix("legacy-install-").unwrap().len(),
            64
        );
        assert_eq!(record["historical"], true);
        assert_eq!(record["instance_id"], id);
        assert_eq!(record["sequence"], SEQUENCE.to_string());
        assert_eq!(record["operation_id"], operation_id);
        assert_eq!(record["command"], "ModifyInstanceContent");
        assert_eq!(record["outcome"], "Succeeded");
        assert!(record.get("failure_point").is_none());
        assert_eq!(record["rollback"], "NotApplicable");
        assert_eq!(record["targets"], operation["targets"]);
        assert_eq!(record["completed_steps"][0]["phase"], "Downloading");
        assert_eq!(record["completed_steps"][0]["result"], "Completed");
        assert_eq!(
            record["completed_steps"][0]["metrics"]["kind"],
            "content_download"
        );
        let counters = record["completed_steps"][0]["metrics"]["values"]
            .as_object()
            .unwrap();
        assert_eq!(counters.len(), 13);
        for key in METRICS {
            assert_eq!(counters[key], u64::MAX.to_string(), "{key}");
        }
        let failed = records
            .iter()
            .find(|record| record["operation_id"] == cancelled_id)
            .unwrap();
        assert_eq!(failed["historical"], true);
        assert_eq!(failed["instance_id"], id);
        assert_eq!(failed["sequence"], (SEQUENCE + 1).to_string());
        assert_eq!(failed["command"], "ModifyInstanceContent");
        assert_eq!(failed["outcome"], "Failed");
        assert_eq!(failed["failure_point"], "content_initialization_cancelled");
        assert_eq!(failed["rollback"], "NotApplicable");
        assert_eq!(failed["targets"], cancelled["targets"]);
        assert_eq!(
            failed["completed_steps"],
            json!([{
                "step_id":"content_progress_initializing","phase":"Failed","result":"Failed",
                "changed_target":null,
                "generated_facts":["install_phase:initializing","install_done:true","install_error:true"],
                "rollback":"NotApplicable","metrics":null
            }])
        );
        for record in records {
            for absent in [
                "status",
                "request",
                "actions",
                "can_cancel",
                "can_retry",
                "created_at",
                "updated_at",
            ] {
                assert!(record.get(absent).is_none(), "{absent}");
            }
        }
        let last_id = records.last().unwrap()["id"].as_str().unwrap();
        assert_eq!(
            get(&services, &format!("{path}&after={last_id}")).await,
            (StatusCode::OK, json!({"records":[],"next_after":null}))
        );
        for after in ["", "../path", "legacy-install-invalid"] {
            assert_eq!(
                get(&services, &format!("{path}&after={after}")).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        for record in records {
            let history_id = record["id"].as_str().unwrap();
            assert_eq!(
                post(
                    &services,
                    &format!("/api/v1/install/{history_id}/cancel"),
                    Value::Null
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                post(
                    &services,
                    &format!("/api/v1/install/queue/retry?expected_install_id={history_id}"),
                    json!({"kind":"vanilla","version_id":"must-not-enqueue"})
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            post(&services, "/api/v1/import/instances", import_request).await,
            (StatusCode::OK, imported.clone())
        );
        assert_eq!(get(&services, &path).await.1, page);
        let duplicate = services
            .instances
            .duplicate(&id.parse().unwrap(), DuplicateRequest::default())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let unmapped = format!("/api/v1/install/history?instance_id={}", duplicate.id);
        assert_eq!(
            get(&services, &unmapped).await,
            (StatusCode::OK, json!({"records":[],"next_after":null}))
        );
        assert_eq!(
            get(&services, &format!("{unmapped}&after=bad")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_no_install_authority(&services);
        assert_eq!(source_snapshot(&baseline), unchanged);
        services.imports.forget().unwrap();
        drop(source);
        assert_eq!(get(&services, &path).await.1, page);
        services.server.shutdown().await.unwrap();
        services.server.wait().await.unwrap();
        drop(services);

        let reopened = crate::start_in_profile(profile, None).await.unwrap();
        assert_eq!(get(&reopened, &path).await, (StatusCode::OK, page));
        assert_eq!(
            get(&reopened, &unmapped).await,
            (StatusCode::OK, json!({"records":[],"next_after":null}))
        );
        assert_no_install_authority(&reopened);
        assert_eq!(source_snapshot(&baseline), unchanged);
        reopened.server.shutdown().await.unwrap();
        reopened.server.wait().await.unwrap();
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
