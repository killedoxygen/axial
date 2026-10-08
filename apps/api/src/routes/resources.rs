//! Resource selectors are resolved only through registered instance authority.

use axial_app::{
    instances::model::InstanceId,
    resources::{
        InstanceLogInfo, ResourceCommand, ResourceError, ResourceService,
        folders::{FolderError, FolderService},
        mods::{InstanceModInfo, UpdateModRequest},
        screenshots::{InstanceScreenshotInfo, RenameScreenshotRequest},
        worlds::{InstanceWorldInfo, RenameWorldRequest},
    },
    tasks::TaskHandle,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router(state: Arc<ResourceService>) -> Router {
    Router::new()
        .route("/api/v1/instances/{id}/resources", get(resources))
        .route("/api/v1/instances/{id}/mods", get(mods))
        .route("/api/v1/instances/{id}/worlds", get(worlds))
        .route("/api/v1/instances/{id}/screenshots", get(screenshots))
        .route("/api/v1/instances/{id}/logs", get(logs))
        .route("/api/v1/instances/{id}/logs/{name}", get(log))
        .route(
            "/api/v1/instances/{id}/mods/{name}",
            put(toggle_mod).delete(delete_mod),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}",
            put(rename_screenshot).delete(delete_screenshot),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}/file",
            get(screenshot_file),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}",
            put(rename_world).delete(delete_world),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}/backup",
            post(backup_world),
        )
        .route("/api/v1/instances/{id}/worlds/{name}/icon", get(world_icon))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(state)
}

type Error = (StatusCode, Json<Value>);

pub fn folders_router(state: Arc<FolderService>) -> Router {
    Router::new()
        .route("/api/v1/instances/{id}/open-folder", post(open_folder))
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderQuery {
    sub: Option<String>,
}

fn folder_error(value: FolderError) -> Error {
    (
        StatusCode::from_u16(value.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error": value.to_string()})),
    )
}

async fn open_folder(
    State(state): State<Arc<FolderService>>,
    Path(raw): Path<String>,
    query: Result<Query<FolderQuery>, QueryRejection>,
) -> Result<Json<Value>, Error> {
    let Query(query) = query.map_err(|_| folder_error(FolderError::InvalidFolder))?;
    state
        .open(&id(&raw)?, query.sub.as_deref())
        .map_err(folder_error)?
        .wait()
        .await
        .map_err(folder_error)?;
    Ok(Json(json!({"status": "ok"})))
}

fn error(value: ResourceError) -> Error {
    (
        StatusCode::from_u16(value.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error":value.to_string()})),
    )
}
fn id(raw: &str) -> Result<InstanceId, Error> {
    raw.parse().map_err(|_| error(ResourceError::InvalidName))
}
fn body<T>(request: Result<Json<T>, JsonRejection>) -> Result<T, Error> {
    request
        .map(|Json(value)| value)
        .map_err(|_| error(ResourceError::InvalidName))
}
async fn command(
    work: Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError>,
) -> Result<Json<ResourceCommand>, Error> {
    work.map_err(error)?
        .join()
        .await
        .map_err(|_| error(ResourceError::Pending))?
        .map(Json)
        .map_err(error)
}

async fn resources(
    State(state): State<Arc<ResourceService>>,
    Path(raw): Path<String>,
) -> Result<Json<axial_app::resources::InstanceResourcesResponse>, Error> {
    let id = id(&raw)?;
    tokio::task::spawn_blocking(move || state.resources(&id))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map(Json)
        .map_err(error)
}
async fn mods(
    State(state): State<Arc<ResourceService>>,
    Path(raw): Path<String>,
) -> Result<Json<Vec<InstanceModInfo>>, Error> {
    let id = id(&raw)?;
    tokio::task::spawn_blocking(move || state.mods(&id))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map(Json)
        .map_err(error)
}
async fn worlds(
    State(state): State<Arc<ResourceService>>,
    Path(raw): Path<String>,
) -> Result<Json<Vec<InstanceWorldInfo>>, Error> {
    let id = id(&raw)?;
    tokio::task::spawn_blocking(move || state.worlds(&id))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map(Json)
        .map_err(error)
}
async fn screenshots(
    State(state): State<Arc<ResourceService>>,
    Path(raw): Path<String>,
) -> Result<Json<Vec<InstanceScreenshotInfo>>, Error> {
    let id = id(&raw)?;
    tokio::task::spawn_blocking(move || state.screenshots(&id))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map(Json)
        .map_err(error)
}
async fn logs(
    State(state): State<Arc<ResourceService>>,
    Path(raw): Path<String>,
) -> Result<Json<Vec<InstanceLogInfo>>, Error> {
    let id = id(&raw)?;
    tokio::task::spawn_blocking(move || state.logs(&id))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map(Json)
        .map_err(error)
}

async fn log(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Json<axial_app::resources::InstanceLogTailResponse>, Error> {
    let id = id(&raw)?;
    tokio::task::spawn_blocking(move || state.log(&id, &name))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map(Json)
        .map_err(error)
}
async fn screenshot_file(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Response, Error> {
    let id = id(&raw)?;
    let media = tokio::task::spawn_blocking(move || state.screenshot(&id, &name))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map_err(error)?;
    Ok((
        [
            (header::CONTENT_TYPE, media.content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        media.bytes,
    )
        .into_response())
}
async fn world_icon(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Response, Error> {
    let id = id(&raw)?;
    let media = tokio::task::spawn_blocking(move || state.world_icon(&id, &name))
        .await
        .map_err(|_| error(ResourceError::Files))?
        .map_err(error)?;
    Ok((
        [
            (header::CONTENT_TYPE, media.content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        media.bytes,
    )
        .into_response())
}
async fn toggle_mod(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
    request: Result<Json<UpdateModRequest>, JsonRejection>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.set_mod_enabled(&id(&raw)?, &name, body(request)?.enabled)).await
}
async fn delete_mod(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.delete_mod(&id(&raw)?, &name)).await
}
async fn rename_screenshot(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
    request: Result<Json<RenameScreenshotRequest>, JsonRejection>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.rename_screenshot(&id(&raw)?, &name, &body(request)?.name)).await
}
async fn delete_screenshot(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.delete_screenshot(&id(&raw)?, &name)).await
}
async fn rename_world(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
    request: Result<Json<RenameWorldRequest>, JsonRejection>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.rename_world(&id(&raw)?, &name, &body(request)?.name)).await
}
async fn delete_world(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.delete_world(&id(&raw)?, &name)).await
}
async fn backup_world(
    State(state): State<Arc<ResourceService>>,
    Path((raw, name)): Path<(String, String)>,
) -> Result<Json<ResourceCommand>, Error> {
    command(state.backup_world(&id(&raw)?, &name)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        content::install::ContentMutations,
        instances::{
            create::InstanceService,
            directory::{InstanceDirectories, Registry},
            model::Instance,
        },
        library::{LibraryId, LibraryLifecycle},
        network::{ClientConfig, ProviderClient},
        resources::folders::{AdmittedFolder, FolderOpener, FolderProcess},
        settings::InstanceSettings,
        storage::MetadataStore,
        tasks::{Exclusions, TaskOwner},
    };
    use axial_fs::{LeafName, RootSession, RootSessionAcquireOutcome};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::{io, path::PathBuf, sync::Mutex, time::Duration};
    use tower::ServiceExt;

    #[derive(Default)]
    struct RecordingOpener(Mutex<Vec<PathBuf>>);

    impl FolderOpener for RecordingOpener {
        fn spawn(&self, folder: &AdmittedFolder) -> Result<Box<dyn FolderProcess>, FolderError> {
            self.0.lock().unwrap().push(folder.checked_path()?);
            Ok(Box::new(FinishedProcess))
        }
    }

    struct FinishedProcess;

    impl FolderProcess for FinishedProcess {
        fn try_wait(&mut self) -> io::Result<bool> {
            Ok(true)
        }

        fn terminate(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FolderFixture {
        _root: tempfile::TempDir,
        id: InstanceId,
        game: PathBuf,
        opener: Arc<RecordingOpener>,
        tasks: TaskOwner,
        app: Router,
    }

    impl FolderFixture {
        fn new() -> Self {
            let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let id = InstanceId::new();
            let game = root.path().join("instances").join(id.as_str());
            std::fs::create_dir_all(&game).unwrap();
            let session = match RootSession::acquire(root.path()) {
                RootSessionAcquireOutcome::Acquired(session) => session,
                other => panic!("folder fixture root: {other:?}"),
            };
            let authority = session.root().unwrap();
            let directory = authority
                .open_directory(&LeafName::new("instances").unwrap())
                .unwrap()
                .open_directory(&LeafName::new(id.as_str()).unwrap())
                .unwrap();
            let hex = |bytes: &[u8]| {
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            };
            let receipt = format!(
                "axial-dir-v1:{}:{}",
                hex(&authority.identity().unwrap().filesystem_witness()),
                hex(&directory.identity().unwrap().filesystem_witness()),
            );
            drop((directory, authority));
            let library_id = LibraryId::new();
            let library = LibraryLifecycle::from_root_session_at(
                session,
                library_id,
                root.path().to_path_buf(),
            )
            .unwrap();
            let metadata = Arc::new(MetadataStore::in_memory().unwrap());
            metadata
                .migrate(&[
                    axial_app::instances::directory::MIGRATION,
                    axial_app::instances::create::MIGRATION,
                    axial_app::instances::delete::MIGRATION,
                    axial_app::content::install::MIGRATION,
                    axial_app::performance::mutation::MIGRATION,
                ])
                .unwrap();
            let registry = Registry::new(metadata.clone());
            metadata
                .transaction(|tx| {
                    let reserved = registry.reserve(
                        tx,
                        Instance {
                            id: id.clone(),
                            name: "Folder route fixture".into(),
                            version_id: "1.21.4".into(),
                            created_at: "2026-01-01T00:00:00Z".into(),
                            last_played_at: String::new(),
                            art_seed: 0,
                            settings: InstanceSettings::default(),
                            java_selection: None,
                            icon: String::new(),
                            accent: String::new(),
                            loader_key: "vanilla".into(),
                            minecraft_version: "1.21.4".into(),
                            revision: 1,
                        },
                        &library_id.to_string(),
                    )?;
                    registry.commit_reserved(tx, &reserved, &receipt)
                })
                .unwrap();
            let tasks = TaskOwner::new(8).unwrap();
            let instances = Arc::new(InstanceService::new(
                InstanceDirectories::new(registry, library, Exclusions::new()),
                tasks.clone(),
            ));
            let directories = instances.directories().clone();
            let content = ContentMutations::new(
                directories.clone(),
                ProviderClient::new(ClientConfig::default()).unwrap(),
                tasks.clone(),
            );
            let opener = Arc::new(RecordingOpener::default());
            let app = folders_router(Arc::new(FolderService::new(
                instances,
                tasks.clone(),
                opener.clone(),
            )))
            .merge(router(Arc::new(ResourceService::new(
                directories,
                content,
                tasks.clone(),
            ))));
            Self {
                _root: root,
                id,
                game,
                opener,
                tasks,
                app,
            }
        }

        async fn request(&self, id: &str, suffix: &str) -> (StatusCode, Value) {
            let response = self
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/v1/instances/{id}/open-folder{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let mut changed = self.tasks.subscribe();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !self.tasks.status().is_idle() {
                    changed.changed().await.unwrap();
                }
            })
            .await
            .expect("fake folder process settles");
            (status, serde_json::from_slice(&body).unwrap())
        }

        async fn resource(&self, name: &str) -> (StatusCode, Value) {
            let response = self
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/v1/instances/{}/{name}", self.id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&body).unwrap())
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn targeted_resource_lists_ignore_an_unrelated_screenshot_symlink() {
        let fixture = FolderFixture::new();
        std::fs::create_dir(fixture.game.join("mods")).unwrap();
        std::fs::write(fixture.game.join("mods/local.jar"), b"regular mod").unwrap();
        std::fs::create_dir(fixture.game.join("logs")).unwrap();
        std::fs::write(fixture.game.join("logs/latest.log"), b"fixture log\n").unwrap();
        let mods_before = fixture.resource("mods").await;
        assert_eq!(mods_before.0, StatusCode::OK);
        assert_eq!(mods_before.1.as_array().unwrap().len(), 1);
        assert_eq!(mods_before.1[0]["name"], "local.jar");
        assert_eq!(mods_before.1[0]["size"], 11);
        assert_eq!(mods_before.1[0]["enabled"], true);
        let logs_before = fixture.resource("logs").await;
        assert_eq!(logs_before.0, StatusCode::OK);
        assert_eq!(logs_before.1.as_array().unwrap().len(), 1);
        assert_eq!(logs_before.1[0]["name"], "latest.log");
        assert_eq!(logs_before.1[0]["size"], 12);

        let outside = fixture._root.path().join("outside-screenshots");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("canary.png"), b"untouched screenshot").unwrap();
        let link = fixture.game.join("screenshots");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let mods_after = fixture.resource("mods").await;
        let logs_after = fixture.resource("logs").await;
        let screenshots = fixture.resource("screenshots").await;
        let resources = fixture.resource("resources").await;
        fixture
            .tasks
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();

        let refused = (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"resource files could not be read or updated"}),
        );
        assert_eq!(screenshots, refused);
        assert_eq!(resources, refused);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), outside);
        assert_eq!(
            std::fs::read(outside.join("canary.png")).unwrap(),
            b"untouched screenshot"
        );
        assert_eq!(mods_after, mods_before);
        assert_eq!(logs_after, logs_before);
    }

    #[tokio::test]
    async fn folders_router_opens_root_and_allowed_subfolders_with_retained_response() {
        let fixture = FolderFixture::new();
        assert_eq!(
            fixture.request(fixture.id.as_str(), "").await,
            (StatusCode::OK, json!({"status": "ok"}))
        );
        let mut expected = vec![fixture.game.clone()];
        for sub in [
            "mods",
            "saves",
            "resourcepacks",
            "shaderpacks",
            "config",
            "screenshots",
            "logs",
        ] {
            assert_eq!(
                fixture
                    .request(fixture.id.as_str(), &format!("?sub={sub}"))
                    .await,
                (StatusCode::OK, json!({"status": "ok"})),
                "subfolder: {sub}"
            );
            assert!(fixture.game.join(sub).is_dir());
            expected.push(fixture.game.join(sub));
        }
        assert_eq!(*fixture.opener.0.lock().unwrap(), expected);
        fixture
            .tasks
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn folders_router_rejects_malformed_extra_and_traversal_queries_before_effects() {
        let fixture = FolderFixture::new();
        for suffix in [
            "?sub=",
            "?sub=mods&sub=logs",
            "?sub%5B%5D=mods",
            "?path=%2Ftmp",
            "?sub=mods&extra=true",
            "?sub=%FF",
            "?sub=%",
            "?sub=..%2Foutside",
            "?sub=%2Ftmp",
            "?sub=mods%2F..%2Flogs",
            "?sub=mods%5C..%5Clogs",
        ] {
            assert_eq!(
                fixture.request(fixture.id.as_str(), suffix).await,
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "invalid instance folder"})
                ),
                "query: {suffix}"
            );
        }
        assert!(fixture.opener.0.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&fixture.game).unwrap().count(), 0);
        assert!(!fixture.game.parent().unwrap().join("outside").exists());
        fixture
            .tasks
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn folders_router_returns_json_for_missing_and_invalid_instance_identities() {
        let fixture = FolderFixture::new();
        assert_eq!(
            fixture
                .request(InstanceId::new().as_str(), "?sub=logs")
                .await,
            (
                StatusCode::NOT_FOUND,
                json!({"error": "instance not found"})
            )
        );
        assert_eq!(
            fixture.request("missing", "").await,
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid resource name or file type"})
            )
        );
        assert!(fixture.opener.0.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&fixture.game).unwrap().count(), 0);
        fixture
            .tasks
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();
    }
}
