//! Authenticated HTTP adaptation of retained content operations.

use axial_app::{
    content::{
        catalog::ContentService,
        compatibility,
        install::{ContentMutations, MutationError},
        model::*,
        resolve::{self, ResolutionSelection},
        view::{self, TargetRef},
    },
    install::{
        model::{InstallQueueContentActionRequest, InstallQueueRequest},
        queue::InstallQueue,
    },
    instances::model::InstanceId,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct ContentRoutes {
    pub service: ContentService,
    pub mutations: ContentMutations,
    pub queue: Arc<InstallQueue>,
}
impl ContentRoutes {
    pub fn new(
        service: ContentService,
        mutations: ContentMutations,
        queue: Arc<InstallQueue>,
    ) -> Self {
        Self {
            service,
            mutations,
            queue,
        }
    }
}

pub fn router(state: Arc<ContentRoutes>) -> Router {
    Router::new()
        .route("/api/v1/content/search", get(search))
        .route("/api/v1/content/item", get(detail))
        .route("/api/v1/content/compatibility", post(compatible))
        .route("/api/v1/content/plan", post(plan))
        .route("/api/v1/content/install", post(install))
        .route("/api/v1/content/modpack/target", get(modpack_target))
        .route("/api/v1/content/modpack/files", get(modpack_files))
        .route("/api/v1/content/modpack/install", post(modpack_install))
        .route("/api/v1/instances/{id}/content", get(installed))
        .route("/api/v1/instances/{id}/content/updates", get(updates))
        .route("/api/v1/instances/{id}/content/uninstall", post(uninstall))
        .route("/api/v1/instances/{id}/content/settle", post(settle))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}

type Error = (StatusCode, Json<Value>);
fn error(status: u16, message: &str) -> Error {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error":message})),
    )
}
fn mutation_error(value: MutationError) -> Error {
    error(value.status_code(), &value.to_string())
}
fn unavailable() -> Error {
    error(503, "Content provider is unavailable. Try again later.")
}
fn id(value: &str) -> Result<InstanceId, Error> {
    value
        .parse()
        .map_err(|_| error(400, "Invalid instance identifier."))
}
fn body<T>(value: Result<Json<T>, JsonRejection>) -> Result<T, Error> {
    value
        .map(|Json(value)| value)
        .map_err(|_| error(400, "Invalid content request."))
}
fn query<T>(value: Result<Query<T>, QueryRejection>) -> Result<T, Error> {
    value
        .map(|Query(value)| value)
        .map_err(|_| error(400, "Invalid content query."))
}

#[derive(Deserialize)]
struct Search {
    kind: ContentKind,
    query: Option<String>,
    loader: Option<String>,
    game_version: Option<String>,
    category: Option<String>,
    sort: Option<SortOrder>,
    offset: Option<u32>,
    limit: Option<u32>,
    instance_id: Option<String>,
}
async fn search(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Query<Search>, QueryRejection>,
) -> Result<Json<Value>, Error> {
    let query = query(request)?;
    let installed = query
        .instance_id
        .as_deref()
        .map(|value| {
            state
                .mutations
                .instance_context(&id(value)?)
                .map_err(mutation_error)
        })
        .transpose()?;
    let response = state
        .service
        .search(&ContentQuery {
            kind: query.kind,
            search: query.query,
            loader: query.loader,
            game_version: query.game_version,
            categories: query.category.into_iter().collect(),
            sort: query.sort.unwrap_or_default(),
            offset: query.offset.unwrap_or(0),
            limit: query.limit.unwrap_or(40).clamp(1, 100),
        })
        .await
        .map_err(|_| unavailable())?;
    let items = response
        .items
        .into_iter()
        .map(|content| {
            let installed = installed.as_ref().is_some_and(|(_, manifest, live)| {
                manifest
                    .find(&content.canonical_id)
                    .is_some_and(|entry| live.contains(entry))
            });
            let mut item = serde_json::to_value(content).expect("content DTO serializes");
            if installed {
                item["install_state"] = json!("installed");
            }
            item
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"items":items,"offset":response.offset,"limit":response.limit,"total":response.total}),
    ))
}

#[derive(Deserialize)]
struct ItemQuery {
    id: String,
}
async fn detail(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Query<ItemQuery>, QueryRejection>,
) -> Result<Json<ContentDetail>, Error> {
    let query = query(request)?;
    state
        .service
        .detail(&CanonicalId(query.id))
        .await
        .map(Json)
        .map_err(|_| unavailable())
}

#[derive(Deserialize)]
struct PackQuery {
    id: String,
    version_id: Option<String>,
}
async fn modpack_target(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Query<PackQuery>, QueryRejection>,
) -> Result<Json<axial_app::instances::from_pack::ModpackTarget>, Error> {
    let query = query(request)?;
    let pack = state
        .mutations
        .resolve_pack(
            &state.service,
            &CanonicalId(query.id),
            query.version_id.as_deref(),
        )
        .await
        .map_err(mutation_error)?;
    Ok(Json(
        axial_app::instances::from_pack::target_from_pack_index(
            pack.canonical_id,
            pack.version_id,
            pack.name,
            pack.archive.index(),
        ),
    ))
}

#[derive(Deserialize)]
struct PackFilesQuery {
    instance_id: String,
    id: String,
    version_id: Option<String>,
}
async fn modpack_files(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Query<PackFilesQuery>, QueryRejection>,
) -> Result<Json<axial_app::content::packs::ModpackFilesPlan>, Error> {
    let query = query(request)?;
    state
        .mutations
        .pack_files(
            &state.service,
            &id(&query.instance_id)?,
            &CanonicalId(query.id),
            query.version_id.as_deref(),
        )
        .await
        .map(Json)
        .map_err(mutation_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackInstall {
    instance_id: String,
    canonical_id: String,
    version_id: Option<String>,
    #[serde(default)]
    selected_file_ids: Vec<String>,
    #[serde(default = "default_true")]
    include_overrides: bool,
}
fn default_true() -> bool {
    true
}

async fn modpack_install(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Json<PackInstall>, JsonRejection>,
) -> Result<Json<axial_app::install::model::InstallQueueStateResponse>, Error> {
    let request = body(request)?;
    if !request.selected_file_ids.is_empty() && request.include_overrides {
        return Err(error(
            400,
            "Modpack overrides cannot be applied with selected files.",
        ));
    }
    let instance = id(&request.instance_id)?;
    let (content, version) = axial_app::content::packs::resolve_pack_version(
        &state.service,
        &CanonicalId(request.canonical_id),
        request.version_id.as_deref(),
    )
    .await
    .map_err(|failure| mutation_error(failure.into()))?;
    state
        .queue
        .enqueue(InstallQueueRequest::Content {
            instance_id: instance.to_string(),
            label: "Install modpack files".into(),
            action: InstallQueueContentActionRequest::Modpack {
                canonical_id: content.canonical_id.0,
                version_id: version.id,
                selected_file_ids: request.selected_file_ids,
                include_overrides: request.include_overrides,
            },
        })
        .await
        .map(Json)
        .map_err(super::install::failure)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selections {
    selections: Vec<ResolutionSelection>,
}
async fn compatible(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Json<Selections>, JsonRejection>,
) -> Result<Json<Value>, Error> {
    let candidates = compatibility::compatibility(&state.service, &body(request)?.selections)
        .await
        .map_err(|_| unavailable())?;
    Ok(Json(json!({"candidates":candidates})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRequest {
    target: TargetRef,
    selections: Vec<ResolutionSelection>,
}
async fn plan(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Json<PlanRequest>, JsonRejection>,
) -> Result<Json<view::ResolutionPlan>, Error> {
    let request = body(request)?;
    let result = match request.target {
        TargetRef::Instance { instance_id } => {
            let plan = state
                .mutations
                .plan(&state.service, &id(&instance_id)?, &request.selections)
                .await
                .map_err(mutation_error)?;
            view::into_plan(plan.resolution(), Some(instance_id), plan.state().target())
        }
        TargetRef::Draft {
            loader,
            game_version,
        } => {
            let target =
                resolve::validated_target(loader.as_deref().unwrap_or("vanilla"), &game_version)
                    .map_err(|_| error(400, "Invalid content target."))?;
            let resolution = view::preview_draft(&state.service, &target, &request.selections)
                .await
                .map_err(|_| unavailable())?;
            view::into_plan(&resolution, None, &target)
        }
    };
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    instance_id: String,
    selections: Vec<ResolutionSelection>,
    #[serde(default)]
    allow_incompatible: bool,
}
async fn install(
    State(state): State<Arc<ContentRoutes>>,
    request: Result<Json<InstallRequest>, JsonRejection>,
) -> Result<Json<axial_app::install::model::InstallQueueStateResponse>, Error> {
    let request = body(request)?;
    let instance = id(&request.instance_id)?;
    state
        .queue
        .enqueue(InstallQueueRequest::Content {
            instance_id: instance.to_string(),
            label: "Install content".into(),
            action: InstallQueueContentActionRequest::Install {
                selections: request.selections,
                allow_incompatible: request.allow_incompatible,
            },
        })
        .await
        .map(Json)
        .map_err(super::install::failure)
}

async fn installed(
    State(state): State<Arc<ContentRoutes>>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, Error> {
    let (_, manifest, live) = state
        .mutations
        .instance_context(&id(&raw)?)
        .map_err(mutation_error)?;
    let entries = manifest.entries().iter().filter(|entry| {
        entry.managed_filename().is_some() && live.contains(entry)
    }).map(|entry| {
        let mut value = json!({
        "canonical_id":entry.canonical_id(),"kind":entry.kind(),"provider":entry.provider(),
        "project_id":entry.project_id(),"version_id":entry.version_id(),
        "filename":entry.managed_filename().map(|name| name.as_str()).unwrap_or(""),"enabled":entry.enabled(),
        });
        if let Some(title) = entry.title() { value["title"] = json!(title); }
        value
    }).collect::<Vec<_>>();
    Ok(Json(json!({"entries":entries})))
}

async fn updates(
    State(state): State<Arc<ContentRoutes>>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, Error> {
    let (target, manifest, live) = state
        .mutations
        .instance_context(&id(&raw)?)
        .map_err(mutation_error)?;
    let updates = resolve::available_updates(&state.service, &target, &manifest, &live)
        .await
        .map_err(|_| unavailable())?;
    Ok(Json(json!({"updates":updates})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Uninstall {
    canonical_ids: Vec<CanonicalId>,
}
async fn uninstall(
    State(state): State<Arc<ContentRoutes>>,
    Path(raw): Path<String>,
    request: Result<Json<Uninstall>, JsonRejection>,
) -> Result<Json<axial_app::install::model::InstallQueueStateResponse>, Error> {
    let instance = id(&raw)?;
    let request = body(request)?;
    state
        .queue
        .enqueue(InstallQueueRequest::Content {
            instance_id: instance.to_string(),
            label: "Remove content".into(),
            action: InstallQueueContentActionRequest::Uninstall {
                canonical_ids: request.canonical_ids.into_iter().map(|id| id.0).collect(),
            },
        })
        .await
        .map(Json)
        .map_err(super::install::failure)
}

async fn settle(
    State(state): State<Arc<ContentRoutes>>,
    Path(raw): Path<String>,
) -> Result<Json<axial_app::content::install::MutationReceipt>, Error> {
    state
        .mutations
        .resume(&id(&raw)?)
        .map_err(mutation_error)?
        .join()
        .await
        .map_err(|_| error(409, "Content operation requires settlement."))?
        .map(Json)
        .map_err(mutation_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        content::provenance::{ContentManifest, MANIFEST_FILE, ManifestEntry},
        install::model::{InstallOutcome, InstallQueueStateResponse},
        instances::{
            directory::{InstanceDirectories, Registry},
            model::Instance,
        },
        library::{LibraryId, LibraryLifecycle},
        network::{ClientConfig, OriginPolicy, ProviderClient},
        settings::InstanceSettings,
        storage::MetadataStore,
        tasks::{Exclusions, TaskOwner},
    };
    use axial_fs::{LeafName, RootSession, RootSessionAcquireOutcome};
    use axum::{body::Body, http::Request, response::IntoResponse};
    use std::{path::PathBuf, sync::Mutex, time::Duration};
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
    use tower::ServiceExt;

    struct Fixture {
        _root: tempfile::TempDir,
        id: InstanceId,
        game: PathBuf,
        app: Router,
        directories: InstanceDirectories,
        queue: Arc<InstallQueue>,
        tasks: TaskOwner,
        requested: Arc<Semaphore>,
        gate: Arc<Semaphore>,
        paths: Arc<Mutex<Vec<String>>>,
        provider: JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let id = InstanceId::new();
            let game = root.path().join("instances").join(id.as_str());
            std::fs::create_dir_all(game.join("resourcepacks")).unwrap();
            let session = match RootSession::acquire(root.path()) {
                RootSessionAcquireOutcome::Acquired(session) => session,
                other => panic!("content route fixture: {other:?}"),
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
            let storage = Arc::new(MetadataStore::in_memory().unwrap());
            storage
                .migrate(&[
                    axial_app::instances::directory::MIGRATION,
                    axial_app::instances::create::MIGRATION,
                    axial_app::instances::create::DUPLICATE_WITNESS_MIGRATION,
                    axial_app::instances::delete::MIGRATION,
                    axial_app::content::install::MIGRATION,
                    axial_app::performance::mutation::MIGRATION,
                    axial_app::install::queue::MIGRATION,
                    axial_app::install::queue::MIGRATION_V2,
                ])
                .unwrap();
            let exclusions = Exclusions::new();
            let tasks = TaskOwner::new(16).unwrap();
            let registry = Registry::new(storage.clone());
            storage
                .transaction(|tx| {
                    let reserved = registry.reserve(
                        tx,
                        Instance {
                            id: id.clone(),
                            name: "Content route fixture".into(),
                            version_id: "1.21.4".into(),
                            created_at: "2026-01-01T00:00:00Z".into(),
                            last_played_at: String::new(),
                            art_seed: 0,
                            settings: InstanceSettings::default(),
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
            let directories =
                InstanceDirectories::new(registry, library.clone(), exclusions.clone());
            let client = ProviderClient::new(ClientConfig::default()).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let requested = Arc::new(Semaphore::new(0));
            let gate = Arc::new(Semaphore::new(0));
            let paths = Arc::new(Mutex::new(Vec::new()));
            let provider_app = Router::new().fallback({
                let requested = requested.clone();
                let gate = gate.clone();
                let paths = paths.clone();
                move |uri: axum::http::Uri| {
                    let requested = requested.clone();
                    let gate = gate.clone();
                    let paths = paths.clone();
                    async move {
                        paths.lock().unwrap().push(uri.path().to_owned());
                        match uri.path() {
                            "/v2/project/owned/version" => Json(json!([])).into_response(),
                            "/v2/project/pack" => Json(json!({
                                "id":"pack", "title":"Fixture pack", "project_type":"modpack"
                            }))
                            .into_response(),
                            "/v2/project/pack/version" => Json(json!([{
                                "project_id":"pack", "id":"pack-v1", "name":"Release",
                                "version_number":"1.0", "version_type":"release",
                                "game_versions":["1.21.4"], "loaders":["minecraft"],
                                "files":[{"filename":"fixture.mrpack", "size":3,
                                    "url":"https://cdn.modrinth.com/fixture.mrpack", "primary":true,
                                    "hashes":{"sha512":"a".repeat(128)}}]
                            }]))
                            .into_response(),
                            _ => {
                                requested.add_permits(1);
                                gate.acquire().await.unwrap().forget();
                                StatusCode::BAD_GATEWAY.into_response()
                            }
                        }
                    }
                }
            });
            let provider = tokio::spawn(async move {
                axum::serve(listener, provider_app).await.unwrap();
            });
            let service = ContentService::with_base_url(
                client.clone(),
                format!("{origin}/v2"),
                OriginPolicy::loopback_for_tests([origin], 0).unwrap(),
            )
            .unwrap();
            let mutations = ContentMutations::new(directories.clone(), client, tasks.clone());
            let queue = Arc::new(
                InstallQueue::new(
                    storage,
                    library,
                    exclusions,
                    tasks.clone(),
                    axial_minecraft::ManagedRuntimeCache::isolated_for_test().unwrap(),
                )
                .unwrap()
                .with_content(Arc::new(service.clone()), Arc::new(mutations.clone())),
            );
            let app = router(Arc::new(ContentRoutes::new(
                service,
                mutations,
                queue.clone(),
            )));
            Self {
                _root: root,
                id,
                game,
                app,
                directories,
                queue,
                tasks,
                requested,
                gate,
                paths,
                provider,
            }
        }

        async fn post(&self, path: &str, value: Value) -> (StatusCode, Value) {
            let response = tokio::time::timeout(
                Duration::from_secs(3),
                self.app.clone().oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from(value.to_string()))
                        .unwrap(),
                ),
            )
            .await
            .expect("content route must not await queued provider or file work")
            .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        }

        async fn get(&self, path: &str) -> (StatusCode, Value) {
            let response = tokio::time::timeout(
                Duration::from_secs(3),
                self.app
                    .clone()
                    .oneshot(Request::get(path).body(Body::empty()).unwrap()),
            )
            .await
            .expect("read-only content route completes while launch retains its lease")
            .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        }

        async fn blocked_install(&self) -> InstallQueueStateResponse {
            let (status, value) = self
                .post(
                    "/api/v1/content/install",
                    json!({"instance_id":self.id,"selections":[{
                        "canonical_id":"modrinth:blocked", "kind":"resource_pack"
                    }]}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let response: InstallQueueStateResponse = serde_json::from_value(value).unwrap();
            assert!(response.started_install.is_some());
            assert!(response.notice.is_none());
            tokio::time::timeout(Duration::from_secs(3), self.requested.acquire())
                .await
                .expect("accepted worker reaches provider")
                .unwrap()
                .forget();
            response
        }

        async fn terminal(&self, id: &str) -> InstallOutcome {
            let (_, mut changed) = self.queue.subscribe();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let status = self.queue.status(id).unwrap();
                    if status.done {
                        return status.outcome.unwrap();
                    }
                    changed.changed().await.unwrap();
                }
            })
            .await
            .expect("accepted content task settles")
        }

        async fn close(self) {
            self.queue.close_admission();
            self.gate.add_permits(32);
            self.tasks.shutdown(Duration::from_secs(3)).await.unwrap();
            self.queue.shutdown_queued().unwrap();
            self.provider.abort();
        }
    }

    #[tokio::test]
    async fn content_reads_during_launch_show_only_live_files_without_rewriting_history() {
        let fixture = Fixture::new().await;
        std::fs::create_dir(fixture.game.join("mods")).unwrap();
        let mut manifest = ContentManifest::default();
        for project in ["owned", "drift", "missing"] {
            manifest
                .try_upsert(ManifestEntry::managed(
                    CanonicalId::for_project(ProviderId::Modrinth, project),
                    ProviderId::Modrinth,
                    project.into(),
                    "v1".into(),
                    ContentKind::Mod,
                    &FileRef {
                        filename: format!("{project}.jar"),
                        url: "https://cdn.modrinth.com/fixture.jar".into(),
                        size: Some(3),
                        sha512: Some(concat!(
                            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2",
                            "192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                        ).into()),
                        sha1: None,
                        primary: true,
                    },
                    Vec::new(),
                    Some(format!("Historical {project}")),
                ).unwrap())
                .unwrap();
        }
        manifest
            .try_upsert(
                ManifestEntry::provenance(
                    CanonicalId("modrinth:pack".into()),
                    ProviderId::Modrinth,
                    "pack".into(),
                    "v1".into(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let raw = manifest.encode_managed().unwrap();
        std::fs::write(fixture.game.join(MANIFEST_FILE), &raw).unwrap();
        std::fs::write(fixture.game.join("mods/owned.jar"), b"abc").unwrap();
        std::fs::write(fixture.game.join("mods/drift.jar"), b"xyz").unwrap();
        let launch = fixture.directories.admit(&fixture.id).unwrap();
        let endpoint = format!("/api/v1/instances/{}/content", fixture.id);
        let (status, listing) = fixture.get(&endpoint).await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
        assert_eq!(listing["entries"][0]["canonical_id"], "modrinth:owned");
        assert_eq!(listing["entries"][0]["filename"], "owned.jar");
        assert_eq!(listing["entries"][0]["title"], "Historical owned");
        let (status, updates) = fixture.get(&format!("{endpoint}/updates")).await;
        assert_eq!(status, StatusCode::OK, "{updates}");
        assert_eq!(updates, json!({"updates":[]}));
        assert_eq!(
            *fixture.paths.lock().unwrap(),
            vec!["/v2/project/owned/version"]
        );
        assert!(fixture.directories.admit(&fixture.id).is_err());
        assert_eq!(
            std::fs::read(fixture.game.join(MANIFEST_FILE)).unwrap(),
            raw
        );
        assert_eq!(
            std::fs::read(fixture.game.join("mods/drift.jar")).unwrap(),
            b"xyz"
        );
        assert!(!fixture.game.join("mods/missing.jar").exists());
        drop(launch);
        fixture.close().await;
    }

    #[tokio::test]
    async fn install_returns_accepted_identity_before_provider_io_and_retains_failure_after_response()
     {
        let fixture = Fixture::new().await;
        let accepted = fixture.blocked_install().await;
        let id = accepted.started_install.unwrap().install_id;
        let status = fixture.queue.status(&id).unwrap();
        assert!(!status.done);
        assert_eq!(status.outcome, None);
        assert_eq!(
            accepted.registry_revision,
            fixture.queue.snapshot().registry_revision
        );
        fixture.gate.add_permits(1);
        assert_eq!(fixture.terminal(&id).await, InstallOutcome::Failed);
        assert!(!fixture.game.join("resourcepacks/blocked.zip").exists());
        fixture.close().await;
    }

    #[tokio::test]
    async fn uninstall_returns_owned_queue_status_and_invalidates_after_real_file_removal() {
        let fixture = Fixture::new().await;
        let mut manifest = ContentManifest::default();
        manifest
            .try_upsert(
                ManifestEntry::managed(
                    CanonicalId("modrinth:owned".into()),
                    ProviderId::Modrinth,
                    "owned".into(),
                    "v1".into(),
                    ContentKind::ResourcePack,
                    &FileRef {
                        filename: "owned.zip".into(),
                        url: "https://cdn.modrinth.com/owned.zip".into(),
                        size: Some(3),
                        sha512: Some(
                            concat!(
                                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2",
                                "192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                            )
                            .into(),
                        ),
                        sha1: None,
                        primary: true,
                    },
                    Vec::new(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        std::fs::write(
            fixture.game.join(MANIFEST_FILE),
            manifest.encode_managed().unwrap(),
        )
        .unwrap();
        std::fs::write(fixture.game.join("resourcepacks/owned.zip"), b"abc").unwrap();
        std::fs::write(fixture.game.join("resourcepacks/user.zip"), b"untouched").unwrap();
        let before = fixture.queue.snapshot().registry_revision;
        let (status, value) = fixture
            .post(
                &format!("/api/v1/instances/{}/content/uninstall", fixture.id),
                json!({"canonical_ids":["modrinth:owned"]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let accepted: InstallQueueStateResponse = serde_json::from_value(value).unwrap();
        let id = accepted.started_install.unwrap().install_id;
        assert_eq!(fixture.terminal(&id).await, InstallOutcome::Succeeded);
        assert!(fixture.queue.snapshot().registry_revision > before);
        assert!(!fixture.game.join("resourcepacks/owned.zip").exists());
        assert_eq!(
            std::fs::read(fixture.game.join("resourcepacks/user.zip")).unwrap(),
            b"untouched"
        );
        assert!(
            ContentManifest::decode_managed(Some(
                &std::fs::read(fixture.game.join(MANIFEST_FILE)).unwrap()
            ))
            .unwrap()
            .is_empty()
        );
        assert!(fixture.paths.lock().unwrap().is_empty());
        fixture.close().await;
    }

    #[tokio::test]
    async fn optional_pack_version_is_pinned_before_accepted_archive_work() {
        let fixture = Fixture::new().await;
        fixture.blocked_install().await;
        let (status, value) = fixture
            .post(
                "/api/v1/content/modpack/install",
                json!({"instance_id":fixture.id,"canonical_id":"modrinth:pack"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let accepted: InstallQueueStateResponse = serde_json::from_value(value).unwrap();
        let action = &accepted.items[0]
            .install_item
            .content
            .as_ref()
            .unwrap()
            .action;
        assert_eq!(
            action,
            &InstallQueueContentActionRequest::Modpack {
                canonical_id: "modrinth:pack".into(),
                version_id: "pack-v1".into(),
                selected_file_ids: Vec::new(),
                include_overrides: true,
            }
        );
        let paths = fixture.paths.lock().unwrap().clone();
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.as_str() == "/v2/project/pack")
                .count(),
            1
        );
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.as_str() == "/v2/project/pack/version")
                .count(),
            1
        );
        assert!(paths.iter().all(|path| !path.ends_with(".mrpack")));
        fixture
            .queue
            .remove(&accepted.items[0].queue_id)
            .await
            .unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn invalid_content_requests_do_not_enqueue_or_contact_the_provider() {
        let fixture = Fixture::new().await;
        for (path, request) in [
            (
                "/api/v1/content/install",
                json!({"instance_id":"invalid", "selections":[]}),
            ),
            (
                "/api/v1/content/install",
                json!({"instance_id":fixture.id, "selections":[]}),
            ),
            (
                "/api/v1/content/install",
                json!({"instance_id":fixture.id, "selections":[], "unknown":true}),
            ),
            (
                "/api/v1/content/modpack/install",
                json!({
                    "instance_id":fixture.id, "canonical_id":"modrinth:pack",
                    "selected_file_ids":["selected"], "include_overrides":true
                }),
            ),
        ] {
            let (status, response) = fixture.post(path, request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        }
        let (status, response) = fixture
            .post(
                &format!("/api/v1/instances/{}/content/uninstall", fixture.id),
                json!({"canonical_ids":["modrinth:duplicate", "modrinth:duplicate"]}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(fixture.queue.snapshot().active.is_none());
        assert!(fixture.queue.snapshot().items.is_empty());
        assert!(fixture.paths.lock().unwrap().is_empty());
        fixture.close().await;
    }

    #[tokio::test]
    async fn invalid_query_is_fixed_bounded_json_without_extractor_details() {
        for raw in [
            "/content/search?kind=secret-private-value",
            "/content/search?kind=mod&limit=no",
        ] {
            let parsed = Query::<Search>::try_from_uri(&raw.parse().unwrap());
            let response = match query(parsed) {
                Err(error) => error.into_response(),
                Ok(_) => panic!("invalid query unexpectedly admitted"),
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()["content-type"], "application/json");
            let bytes = axum::body::to_bytes(response.into_body(), 256)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"error":"Invalid content query."})
            );
        }
    }
}
