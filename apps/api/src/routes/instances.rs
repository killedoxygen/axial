use crate::{
    application::instances::{
        self, CreateInstanceRequest, CreateInstanceResponse, CreateInstanceViewResponse,
        CreateLoaderBuildsViewResponse, DuplicateInstanceRequest, InstanceLogInfo,
        InstanceLogTailResponse, InstanceModInfo, InstancePatch, InstanceResourcesResponse,
        InstanceScreenshotInfo, InstanceWorldInfo, OpenFolderQuery, RenameScreenshotRequest,
        RenameWorldRequest, UpdateModRequest, WorldBackupResponse,
    },
    state::{AppState, RequestProducerHandoff},
};
use axial_config::EnrichedInstance;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::Response,
    routing::{get, post, put},
};
use std::collections::HashMap;

#[derive(Debug, Default, serde::Deserialize)]
struct CreateInstanceViewQuery {
    source: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CreateLoaderBuildsViewQuery {
    #[serde(default)]
    source: String,
    #[serde(default)]
    minecraft_version: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/instances",
            get(handle_list_instances).post(handle_create_instance),
        )
        .route(
            "/api/v1/instances/create-view",
            get(handle_create_instance_view),
        )
        .route(
            "/api/v1/instances/create-view/loader-builds",
            get(handle_create_loader_builds_view),
        )
        .route(
            "/api/v1/instances/{id}",
            get(handle_get_instance)
                .put(handle_update_instance)
                .delete(handle_delete_instance),
        )
        .route(
            "/api/v1/instances/{id}/duplicate",
            post(handle_duplicate_instance),
        )
        .route(
            "/api/v1/instances/{id}/resources",
            get(handle_instance_resources),
        )
        .route("/api/v1/instances/{id}/worlds", get(handle_instance_worlds))
        .route(
            "/api/v1/instances/{id}/worlds/{name}",
            put(handle_rename_instance_world).delete(handle_delete_instance_world),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}/backup",
            post(handle_backup_instance_world),
        )
        .route("/api/v1/instances/{id}/mods", get(handle_instance_mods))
        .route(
            "/api/v1/instances/{id}/mods/{name}",
            put(handle_update_instance_mod).delete(handle_delete_instance_mod),
        )
        .route(
            "/api/v1/instances/{id}/screenshots",
            get(handle_instance_screenshots),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}",
            put(handle_rename_instance_screenshot).delete(handle_delete_instance_screenshot),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}/file",
            get(handle_instance_screenshot_file),
        )
        .route("/api/v1/instances/{id}/logs", get(handle_instance_logs))
        .route(
            "/api/v1/instances/{id}/logs/{name}",
            get(handle_instance_log_tail),
        )
        .route(
            "/api/v1/instances/{id}/open-folder",
            post(handle_open_instance_folder),
        )
}

async fn handle_list_instances(
    State(state): State<AppState>,
) -> Json<instances::InstancesResponse> {
    Json(instances::handle_list_instances(&state).await)
}

async fn handle_get_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_get_instance(&state, &id).await.map(Json)
}

async fn handle_create_instance_view(
    State(state): State<AppState>,
    Query(query): Query<CreateInstanceViewQuery>,
) -> Json<CreateInstanceViewResponse> {
    Json(instances::handle_create_instance_view(&state, query.source.as_deref()).await)
}

async fn handle_create_loader_builds_view(
    State(state): State<AppState>,
    Query(query): Query<CreateLoaderBuildsViewQuery>,
) -> Result<Json<CreateLoaderBuildsViewResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_create_loader_builds_view(&state, &query.source, &query.minecraft_version)
        .await
        .map(Json)
}

async fn handle_create_instance(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<CreateInstanceRequest>,
) -> Result<Json<CreateInstanceResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_create_instance_owned(&state, payload, handoff)
        .await
        .map(Json)
}

async fn handle_duplicate_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Option<Json<DuplicateInstanceRequest>>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_duplicate_instance(&state, &id, payload.map(|Json(payload)| payload))
        .await
        .map(Json)
}

async fn handle_update_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<InstancePatch>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_update_instance(&state, &id, patch)
        .await
        .map(Json)
}

async fn handle_open_instance_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<OpenFolderQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_open_instance_folder(&state, &id, query)
        .await
        .map(Json)
}

async fn handle_instance_resources(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceResourcesResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_resources(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_worlds(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceWorldInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_worlds(&state, &id)
        .await
        .map(Json)
}

async fn handle_rename_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<RenameWorldRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_rename_instance_world(&state, &id, &name, payload)
        .await
        .map(Json)
}

async fn handle_delete_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance_world(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_backup_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<WorldBackupResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_backup_instance_world(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_instance_mods(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceModInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_mods(&state, &id).await.map(Json)
}

async fn handle_update_instance_mod(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<UpdateModRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_update_instance_mod(&state, &id, &name, payload)
        .await
        .map(Json)
}

async fn handle_delete_instance_mod(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance_mod(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_instance_screenshots(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceScreenshotInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_screenshots(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_screenshot_file(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_screenshot_file(&state, &id, &name).await
}

async fn handle_rename_instance_screenshot(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<RenameScreenshotRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_rename_instance_screenshot(&state, &id, &name, payload)
        .await
        .map(Json)
}

async fn handle_delete_instance_screenshot(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance_screenshot(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_instance_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceLogInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_logs(&state, &id).await.map(Json)
}

async fn handle_instance_log_tail(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<InstanceLogTailResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_log_tail(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_delete_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance(&state, &id, query)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceStore};
    use axial_performance::PerformanceManager;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use serde_json::{Value, json};
    use std::{
        fs,
        path::{Path as FsPath, PathBuf},
        sync::Arc,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn create_view_route_serializes_backend_preset_options() {
        let fixture = RouteInstanceFixture::new("create-view-route");

        let (status, payload) = fixture
            .request_json(Method::GET, "/api/v1/instances/create-view", None)
            .await;

        assert_eq!(status, StatusCode::OK);
        let preset_options = payload["preset_options"]
            .as_array()
            .expect("preset options array");
        assert!(
            preset_options
                .iter()
                .any(|option| option["id"] == "" && option["default"] == true)
        );
        assert!(preset_options.iter().any(|option| {
            option["id"] == "performance"
                && option["label"]
                    .as_str()
                    .is_some_and(|label| !label.is_empty())
                && option["detail"]
                    .as_str()
                    .is_some_and(|detail| !detail.is_empty())
        }));
    }

    #[tokio::test]
    async fn create_instance_route_resets_unknown_jvm_preset_without_echoing_raw_value() {
        let fixture = RouteInstanceFixture::new("create-route-unknown-preset");
        fixture.configure_create_manifest(&["1.21.1"]);

        let (status, payload) = fixture
            .request_json(
                Method::POST,
                "/api/v1/instances",
                Some(json!({
                    "name": "Route preset tamper",
                    "selection_id": "vanilla|1.21.1",
                    "jvm_preset_id": "C:\\Users\\Alice\\java.exe --accessToken raw-secret-token"
                })),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["name"], "Route preset tamper");
        assert_eq!(payload["jvm_preset"], "");
        assert_eq!(
            payload["guardian_notice"]["state_id"],
            "unknown_reset_to_auto"
        );
        assert_eq!(payload["result"]["command"], "CreateInstance");
        assert_no_route_sensitive_fragments(&payload);
    }

    #[tokio::test]
    async fn create_instance_route_rejects_raw_version_id_without_echoing_raw_value() {
        let fixture = RouteInstanceFixture::new("create-route-legacy-version-id");
        fixture.configure_create_manifest(&["1.21.1"]);

        let (status, payload) = fixture
            .request_json(
                Method::POST,
                "/api/v1/instances",
                Some(json!({
                    "name": "Raw selector",
                    "version_id": "C:\\Users\\Alice\\java.exe --accessToken raw-secret-token"
                })),
            )
            .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(payload, json!({ "error": "selection_id is required" }));
        assert_no_route_sensitive_fragments(&payload);
    }

    #[tokio::test]
    async fn update_instance_route_redacts_java_path_and_jvm_args() {
        let fixture = RouteInstanceFixture::new("update-route-runtime-redaction");
        let instance = fixture
            .state
            .instances()
            .add(
                "Route override".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");

        let (status, payload) = fixture
            .request_json(
                Method::PUT,
                &format!("/api/v1/instances/{}", instance.id),
                Some(json!({
                    "java_path": "C:\\Users\\Alice\\.jdks\\bad\\bin\\java.exe",
                    "extra_jvm_args": "-Dtoken=raw-secret-token -javaagent:C:\\Users\\Alice\\agent.jar"
                })),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["java_path"], "");
        assert_eq!(payload["extra_jvm_args"], "");
        assert_no_route_sensitive_fragments(&payload);
        let stored = fixture
            .state
            .instances()
            .get(&instance.id)
            .expect("stored instance");
        assert!(stored.java_path.contains("java.exe"));
        assert!(stored.extra_jvm_args.contains("raw-secret-token"));
    }

    struct RouteInstanceFixture {
        state: AppState,
        root: PathBuf,
    }

    impl RouteInstanceFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        fn configure_create_manifest(&self, version_ids: &[&str]) {
            let library_dir = self.root.join("library");
            self.state
                .set_library_dir(library_dir.to_string_lossy().to_string());
            write_route_version_manifest_cache(&library_dir, version_ids);
        }

        async fn request_json(
            &self,
            method: Method,
            uri: &str,
            payload: Option<Value>,
        ) -> (StatusCode, Value) {
            let request_lease = self
                .state
                .try_admit_request()
                .expect("admit route instance request");
            let mut request = Request::builder()
                .extension(request_lease.producer_handoff())
                .method(method)
                .uri(uri);
            let body = if let Some(payload) = payload {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&payload).expect("serialize request"))
            } else {
                Body::empty()
            };
            let response = router()
                .with_state(self.state.clone())
                .oneshot(request.body(body).expect("request"))
                .await
                .expect("route response");
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            let payload = serde_json::from_slice(&body).expect("json response");
            (status, payload)
        }
    }

    fn write_route_version_manifest_cache(library_dir: &FsPath, version_ids: &[&str]) {
        let cache_path = axial_minecraft::version_manifest_cache_path(library_dir);
        fs::create_dir_all(cache_path.parent().expect("version manifest cache parent"))
            .expect("create version manifest cache dir");
        let versions = version_ids
            .iter()
            .enumerate()
            .map(|(index, version_id)| {
                json!({
                    "id": version_id,
                    "type": "release",
                    "url": format!("https://example.invalid/{version_id}.json"),
                    "time": format!("2026-01-{:02}T00:00:00+00:00", index + 1),
                    "releaseTime": format!("2026-01-{:02}T00:00:00+00:00", index + 1),
                    "sha1": "",
                    "complianceLevel": 1
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            cache_path,
            serde_json::to_vec_pretty(&json!({
                "latest": {
                    "release": version_ids.first().copied().unwrap_or("1.21.1"),
                    "snapshot": version_ids.last().copied().unwrap_or("1.21.1")
                },
                "versions": versions
            }))
            .expect("serialize version manifest cache"),
        )
        .expect("write version manifest cache");
    }

    impl Drop for RouteInstanceFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-instance-route-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &FsPath) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn assert_no_route_sensitive_fragments(value: &Value) {
        let text = value.to_string();
        for material in [
            "Alice",
            "java.exe",
            "accessToken",
            "raw-secret-token",
            "javaagent",
            "agent.jar",
            "C:\\Users",
            "AppData",
        ] {
            assert!(
                !text.contains(material),
                "public instance route JSON exposed sensitive material {material}: {text}"
            );
        }
    }
}
