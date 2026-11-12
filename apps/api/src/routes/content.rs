//! Authenticated HTTP adaptation of retained content operations.

use super::error::{ApiError, ApiResult, Extracted};
use axial_app::{
    content::{
        catalog::{ContentError, ContentService},
        compatibility,
        install::{ContentMutations, MutationError, MutationReceipt},
        model::*,
        packs::{ModpackFilesPlan, PackError, PackVersion},
        resolve::{self, ResolutionSelection},
        view::{
            self, CompatibilityResponse, InstalledContent, ResolutionPlan, SearchItem, TargetRef,
            UpdatesResponse,
        },
    },
    install::{model::InstallQueueStateResponse, queue::InstallQueue},
    instances::{from_pack::ModpackTarget, model::InstanceId},
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
use std::sync::Arc;

pub struct ContentRoutes {
    pub service: ContentService,
    pub mutations: ContentMutations,
    pub queue: Arc<InstallQueue>,
}

type Content = State<Arc<ContentRoutes>>;

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
        // The archive body is bounded by the handler's own read limit.
        .route("/api/v1/content/modpack/file", post(modpack_file))
        .with_state(state)
}

const MAX_MODPACK_FILE_BYTES: usize = 128 * 1024 * 1024;

fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Content provider is unavailable. Try again later.",
    )
}
/// Why a provider read failed, without its request or response detail.
fn read_failure(error: ContentError) -> ApiError {
    match error {
        ContentError::Invalid(_) => invalid(INVALID_QUERY),
        ContentError::Parse(_) | ContentError::ProviderMetadataInvalid(_) => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "Modrinth sent a response Axial could not read. Try again later.",
        ),
        ContentError::Provider(_) | ContentError::Unavailable => unavailable(),
    }
}
fn id(value: &str) -> Result<InstanceId, ApiError> {
    value
        .parse()
        .map_err(|_| invalid("Invalid instance identifier."))
}
const INVALID_QUERY: &str = "Invalid content query.";
const INVALID_REQUEST: &str = "Invalid content request.";

/// Read a local `.mrpack` as its published Modrinth version or as a local
/// pack; creation then uses the ordinary modpack route.
async fn modpack_file(State(state): Content, body: axum::body::Body) -> ApiResult<ModpackTarget> {
    let bytes = axum::body::to_bytes(body, MAX_MODPACK_FILE_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "This modpack is larger than the 128 MB import limit.",
            )
        })?;
    axial_app::instances::from_pack::modpack_file_target(
        &state.service,
        &state.mutations,
        bytes.to_vec(),
    )
    .await
    .map(Json)
    .map_err(|failure| match failure {
        PackError::DisallowedHost => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "This modpack downloads files from a site other than Modrinth, GitHub or GitLab, so it can't be imported.",
        ),
        PackError::Provider | PackError::Download => unavailable(),
        PackError::TooLarge => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "This modpack exceeds the supported size limit.",
        ),
        _ => invalid("This file is not a readable Modrinth modpack (.mrpack)."),
    })
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
    State(state): Content,
    request: Result<Query<Search>, QueryRejection>,
) -> ApiResult<Page<SearchItem>> {
    let query = request.or_refuse(|| invalid(INVALID_QUERY))?;
    let instance = query.instance_id.as_deref().map(id).transpose()?;
    view::search(
        &state.service,
        &state.mutations,
        &ContentQuery {
            kind: query.kind,
            search: query.query,
            loader: query.loader,
            game_version: query.game_version,
            categories: query.category.into_iter().collect(),
            sort: query.sort.unwrap_or_default(),
            offset: query.offset.unwrap_or(0),
            limit: query.limit.unwrap_or(40).clamp(1, 100),
        },
        instance.as_ref(),
    )
    .await
    .map(Json)
    .map_err(read_failure)
}

#[derive(Deserialize)]
struct ItemQuery {
    id: String,
}
async fn detail(
    State(state): Content,
    request: Result<Query<ItemQuery>, QueryRejection>,
) -> ApiResult<ContentDetail> {
    let query = request.or_refuse(|| invalid(INVALID_QUERY))?;
    state
        .service
        .detail(&CanonicalId(query.id))
        .await
        .map(Json)
        .map_err(read_failure)
}

#[derive(Deserialize)]
struct PackQuery {
    id: String,
    version_id: Option<String>,
    game_version: Option<String>,
    loader: Option<String>,
}
async fn modpack_target(
    State(state): Content,
    request: Result<Query<PackQuery>, QueryRejection>,
) -> ApiResult<ModpackTarget> {
    let query = request.or_refuse(|| invalid(INVALID_QUERY))?;
    let filter = LoaderGameFilter {
        loader: query.loader,
        game_version: query.game_version,
    };
    let version = query
        .version_id
        .as_deref()
        .map_or(PackVersion::Newest(&filter), PackVersion::Exact);
    let pack = state
        .mutations
        .resolve_pack(&state.service, &CanonicalId(query.id), version)
        .await?;
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
    State(state): Content,
    request: Result<Query<PackFilesQuery>, QueryRejection>,
) -> ApiResult<ModpackFilesPlan> {
    let query = request.or_refuse(|| invalid(INVALID_QUERY))?;
    Ok(Json(
        state
            .mutations
            .pack_files(
                &state.service,
                &id(&query.instance_id)?,
                &CanonicalId(query.id),
                query.version_id.as_deref(),
            )
            .await?,
    ))
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
    State(state): Content,
    request: Result<Json<PackInstall>, JsonRejection>,
) -> ApiResult<InstallQueueStateResponse> {
    let request = request.or_refuse(|| invalid(INVALID_REQUEST))?;
    if !request.selected_file_ids.is_empty() && request.include_overrides {
        return Err(invalid(
            "Modpack overrides cannot be applied with selected files.",
        ));
    }
    let instance = id(&request.instance_id)?;
    let any = LoaderGameFilter::default();
    let version = request
        .version_id
        .as_deref()
        .map_or(PackVersion::Newest(&any), PackVersion::Exact);
    let (content, version) = axial_app::content::packs::resolve_pack_version(
        &state.service,
        &CanonicalId(request.canonical_id),
        version,
    )
    .await
    .map_err(MutationError::from)?;
    let queued = ContentMutations::pack_files_request(
        &instance,
        content.canonical_id,
        version.id,
        request.selected_file_ids,
        request.include_overrides,
    );
    Ok(Json(state.queue.enqueue(queued).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selections {
    selections: Vec<ResolutionSelection>,
}
async fn compatible(
    State(state): Content,
    request: Result<Json<Selections>, JsonRejection>,
) -> ApiResult<CompatibilityResponse> {
    let request = request.or_refuse(|| invalid(INVALID_REQUEST))?;
    let candidates = compatibility::compatibility(&state.service, &request.selections)
        .await
        .map_err(|_| unavailable())?;
    Ok(Json(CompatibilityResponse { candidates }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRequest {
    target: TargetRef,
    selections: Vec<ResolutionSelection>,
}
async fn plan(
    State(state): Content,
    request: Result<Json<PlanRequest>, JsonRejection>,
) -> ApiResult<ResolutionPlan> {
    let request = request.or_refuse(|| invalid(INVALID_REQUEST))?;
    Ok(Json(match request.target {
        TargetRef::Instance { instance_id } => {
            let plan = state
                .mutations
                .plan(&state.service, &id(&instance_id)?, &request.selections)
                .await?;
            view::into_instance_plan(&plan, instance_id)
        }
        TargetRef::Draft {
            loader,
            game_version,
        } => {
            let target =
                resolve::validated_target(loader.as_deref().unwrap_or("vanilla"), &game_version)
                    .map_err(|_| invalid("Invalid content target."))?;
            let resolution = view::preview_draft(&state.service, &target, &request.selections)
                .await
                .map_err(|_| unavailable())?;
            view::into_plan(&resolution, None, &target)
        }
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    instance_id: String,
    selections: Vec<ResolutionSelection>,
    #[serde(default)]
    allow_incompatible: bool,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    replace_files: Vec<String>,
}
async fn install(
    State(state): Content,
    request: Result<Json<InstallRequest>, JsonRejection>,
) -> ApiResult<InstallQueueStateResponse> {
    let request = request.or_refuse(|| invalid(INVALID_REQUEST))?;
    let instance = id(&request.instance_id)?;
    let queued = state.mutations.install_request(
        &instance,
        request.selections,
        request.allow_incompatible,
        request.replace_files,
        request.label.as_deref(),
    );
    Ok(Json(state.queue.enqueue(queued).await?))
}

async fn installed(State(state): Content, Path(raw): Path<String>) -> ApiResult<InstalledContent> {
    let (_, manifest, live) = state.mutations.instance_context(&id(&raw)?)?;
    Ok(Json(view::installed(&manifest, &live)))
}

async fn updates(State(state): Content, Path(raw): Path<String>) -> ApiResult<UpdatesResponse> {
    let (target, manifest, live) = state.mutations.instance_context(&id(&raw)?)?;
    let updates = resolve::available_updates(&state.service, &target, &manifest, &live)
        .await
        .map_err(|_| unavailable())?;
    Ok(Json(UpdatesResponse { updates }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Uninstall {
    canonical_ids: Vec<CanonicalId>,
}
async fn uninstall(
    State(state): Content,
    Path(raw): Path<String>,
    request: Result<Json<Uninstall>, JsonRejection>,
) -> ApiResult<InstallQueueStateResponse> {
    let instance = id(&raw)?;
    let request = request.or_refuse(|| invalid(INVALID_REQUEST))?;
    let queued = state
        .mutations
        .uninstall_request(&instance, request.canonical_ids)
        .map_err(|refusal| ApiError::body(StatusCode::CONFLICT, refusal))?;
    Ok(Json(state.queue.enqueue(queued).await?))
}

async fn settle(State(state): Content, Path(raw): Path<String>) -> ApiResult<MutationReceipt> {
    let work = state.mutations.resume(&id(&raw)?)?;
    let receipt = work.join().await.map_err(|_| {
        ApiError::new(
            StatusCode::CONFLICT,
            "Content operation requires settlement.",
        )
    })??;
    Ok(Json(receipt))
}

#[cfg(test)]
mod tests {
    use crate::{TestInputs, fixture::Profile};
    use axial_app::{
        content::{
            model::{CanonicalId, ContentKind, FileRef, ProviderId},
            provenance::{ContentManifest, MANIFEST_FILE, ManifestEntry},
        },
        install::model::{
            InstallOutcome, InstallQueueContentActionRequest, InstallQueueStateResponse,
        },
        instances::model::InstanceId,
    };
    use axum::{Json, Router, http::StatusCode, response::IntoResponse};
    use serde_json::{Value, json};
    use std::{path::PathBuf, sync::Arc, sync::Mutex, time::Duration};
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};

    const SHA512_ABC: &str = concat!(
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2",
        "192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );

    fn release(project: &str, file: &str, dependencies: Value) -> Value {
        json!([{
            "project_id":project, "id":format!("{project}-v2"), "name":"Release",
            "version_number":"2.0", "version_type":"release",
            "game_versions":["1.20.1"], "loaders":["minecraft"], "dependencies":dependencies,
            "files":[{"filename":file, "size":3, "url":format!("https://cdn.modrinth.com/{file}"),
                "primary":true, "hashes":{"sha512":"a".repeat(128)}}]
        }])
    }

    /// A production profile whose content provider is this loopback fixture.
    /// Unknown provider paths hold until the test releases them.
    struct Fixture {
        profile: Profile,
        id: InstanceId,
        game: PathBuf,
        requested: Arc<Semaphore>,
        gate: Arc<Semaphore>,
        paths: Arc<Mutex<Vec<String>>>,
        provider: JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let requested = Arc::new(Semaphore::new(0));
            let gate = Arc::new(Semaphore::new(0));
            let paths = Arc::new(Mutex::new(Vec::new()));
            let provider_app = Router::new().fallback({
                let (requested, gate, paths) = (requested.clone(), gate.clone(), paths.clone());
                move |uri: axum::http::Uri| {
                    let (requested, gate, paths) = (requested.clone(), gate.clone(), paths.clone());
                    async move {
                        paths.lock().unwrap().push(uri.path().to_owned());
                        match uri.path() {
                            "/v2/search" => Json(json!({
                                "hits":[{"project_id":"owned", "title":"Fixture resource pack",
                                    "project_type":"resourcepack"}],
                                "offset":0, "limit":1, "total_hits":1
                            }))
                            .into_response(),
                            "/v2/project/owned/version" | "/v2/versions" => {
                                Json(json!([])).into_response()
                            }
                            "/v2/project/updateable/version" => {
                                Json(release("updateable", "updateable-v2.zip", json!([])))
                                    .into_response()
                            }
                            "/v2/project/dependent/version" => Json(release(
                                "dependent",
                                "dependent-v2.zip",
                                json!([{"project_id":null,"version_id":"unresolved-pin",
                                    "dependency_type":"incompatible"}]),
                            ))
                            .into_response(),
                            "/v2/project/removed/version" => StatusCode::NOT_FOUND.into_response(),
                            "/v2/project/unavailable/version" => {
                                StatusCode::SERVICE_UNAVAILABLE.into_response()
                            }
                            "/v2/project/pack" => Json(json!({
                                "id":"pack", "title":"Fixture pack", "project_type":"modpack"
                            }))
                            .into_response(),
                            "/v2/project/pack/version" => {
                                Json(release("pack", "fixture.mrpack", json!([]))).into_response()
                            }
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
            let profile = Profile::start_with(TestInputs {
                content: Some((
                    format!("{origin}/v2"),
                    axial_app::performance::public_transfer_resolver(),
                )),
                ..TestInputs::default()
            })
            .await;
            let (id, game) = profile.create_instance().await;
            std::fs::create_dir_all(game.join("resourcepacks")).unwrap();
            paths.lock().unwrap().clear();
            Self {
                profile,
                id,
                game,
                requested,
                gate,
                paths,
                provider,
            }
        }

        async fn post(&self, path: &str, value: Value) -> (StatusCode, Value) {
            tokio::time::timeout(
                Duration::from_secs(3),
                self.profile
                    .api
                    .response(reqwest::Method::POST, path, Some(value)),
            )
            .await
            .expect("content route must not await queued provider or file work")
        }

        async fn get(&self, path: &str) -> (StatusCode, Value) {
            tokio::time::timeout(Duration::from_secs(3), self.profile.api.read(path))
                .await
                .expect("read-only content route completes while launch retains its lease")
        }

        fn queue(&self) -> &axial_app::install::queue::InstallQueue {
            &self.profile.services.installs
        }

        async fn blocked_install(&self) -> InstallQueueStateResponse {
            let (status, value) = self
                .post(
                    "/api/v1/content/install",
                    json!({"instance_id":self.id,"label":" Blocked\n pack ","selections":[{
                        "canonical_id":"modrinth:blocked", "kind":"resource_pack"
                    }]}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let response: InstallQueueStateResponse = serde_json::from_value(value).unwrap();
            assert_eq!(queued_label(&response), "Install Blocked pack");
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
            let (_, mut changed) = self.queue().subscribe();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let status = self.queue().status(id).unwrap();
                    if status.done {
                        return status.outcome.unwrap();
                    }
                    changed.changed().await.unwrap();
                }
            })
            .await
            .expect("accepted content task settles")
        }

        fn write_manifest(&self, entries: &[ManifestEntry], files: &[(&str, &[u8])]) -> Vec<u8> {
            let mut manifest = ContentManifest::default();
            for entry in entries {
                manifest.try_upsert(entry.clone()).unwrap();
            }
            let raw = manifest.encode_managed().unwrap();
            std::fs::write(self.game.join(MANIFEST_FILE), &raw).unwrap();
            for (path, bytes) in files {
                std::fs::write(self.game.join(path), bytes).unwrap();
            }
            raw
        }

        fn write_managed_resource_packs(&self, projects: &[&str]) -> Vec<u8> {
            let entries = projects
                .iter()
                .map(|project| {
                    managed(
                        project,
                        ContentKind::ResourcePack,
                        &format!("{project}.zip"),
                        &format!("Installed {project}"),
                    )
                })
                .collect::<Vec<_>>();
            let files = projects
                .iter()
                .map(|project| (format!("resourcepacks/{project}.zip"), b"abc".as_slice()))
                .chain([("resourcepacks/user.zip".to_owned(), b"untouched".as_slice())])
                .collect::<Vec<_>>();
            let files = files
                .iter()
                .map(|(path, bytes)| (path.as_str(), *bytes))
                .collect::<Vec<_>>();
            self.write_manifest(&entries, &files)
        }

        async fn close(self) {
            self.gate.add_permits(32);
            self.profile.close().await;
            self.provider.abort();
        }
    }

    fn managed(project: &str, kind: ContentKind, filename: &str, title: &str) -> ManifestEntry {
        ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.into(),
            "v1".into(),
            kind,
            &FileRef {
                filename: filename.into(),
                url: format!("https://cdn.modrinth.com/{filename}"),
                size: Some(3),
                sha512: Some(SHA512_ABC.into()),
                sha1: None,
                primary: true,
            },
            Vec::new(),
            Some(title.into()),
        )
        .unwrap()
    }

    fn queued_label(response: &InstallQueueStateResponse) -> &str {
        response
            .active
            .as_ref()
            .map(|active| active.label.as_str())
            .or_else(|| response.items.first().map(|item| item.label.as_str()))
            .expect("accepted content work is visible in the queue")
    }

    #[tokio::test]
    async fn discovery_survives_unreadable_content_without_waiving_mutation_refusal() {
        let fixture = Fixture::new().await;
        let mut malformed = fixture.write_managed_resource_packs(&["owned"]);
        let search = "/api/v1/content/search?kind=resource_pack&limit=1";
        let targeted_search = format!("{search}&instance_id={}", fixture.id);
        let (provider_status, provider_page) = fixture.get(search).await;
        let (installed_status, installed_page) = fixture.get(&targeted_search).await;
        malformed.push(b'!');
        std::fs::write(fixture.game.join(MANIFEST_FILE), &malformed).unwrap();

        let (search_status, search_page) = fixture.get(&targeted_search).await;
        let (missing_status, missing_page) = fixture
            .get(&format!("{search}&instance_id={}", InstanceId::new()))
            .await;
        let (invalid_status, _) = fixture.get(&format!("{search}&instance_id=invalid")).await;
        let (read_status, read_response) = fixture
            .get(&format!("/api/v1/instances/{}/content", fixture.id))
            .await;
        let selections = json!([{"canonical_id":"modrinth:owned", "kind":"resource_pack"}]);
        let (plan_status, plan_response) = fixture
            .post(
                "/api/v1/content/plan",
                json!({"target":{"kind":"instance", "instance_id":fixture.id},
                    "selections":selections}),
            )
            .await;
        let (install_status, install_response) = fixture
            .post(
                "/api/v1/content/install",
                json!({"instance_id":fixture.id, "selections":selections}),
            )
            .await;
        assert_eq!(install_status, StatusCode::OK, "{install_response}");
        let accepted: InstallQueueStateResponse = serde_json::from_value(install_response).unwrap();
        let install_id = accepted.started_install.unwrap().install_id;
        assert_eq!(fixture.terminal(&install_id).await, InstallOutcome::Failed);
        assert_eq!(
            std::fs::read(fixture.game.join(MANIFEST_FILE)).unwrap(),
            malformed
        );
        assert_eq!(
            std::fs::read(fixture.game.join("resourcepacks/owned.zip")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read(fixture.game.join("resourcepacks/user.zip")).unwrap(),
            b"untouched"
        );

        assert_eq!(provider_status, StatusCode::OK, "{provider_page}");
        assert_eq!(provider_page["items"].as_array().unwrap().len(), 1);
        assert_eq!(provider_page["items"][0]["canonical_id"], "modrinth:owned");
        assert_eq!(provider_page["items"][0]["title"], "Fixture resource pack");
        assert_eq!(provider_page["total"], 1);
        assert!(provider_page["items"][0].get("install_state").is_none());
        assert_eq!(installed_status, StatusCode::OK, "{installed_page}");
        assert_eq!(installed_page["items"][0]["install_state"], "installed");
        assert_eq!(read_status, StatusCode::BAD_REQUEST, "{read_response}");
        assert_eq!(plan_status, StatusCode::BAD_REQUEST, "{plan_response}");
        assert_eq!(invalid_status, StatusCode::BAD_REQUEST);
        for (status, page) in [(search_status, search_page), (missing_status, missing_page)] {
            assert_eq!((status, page), (StatusCode::OK, provider_page.clone()));
        }
        fixture.close().await;
    }

    #[tokio::test]
    async fn content_reads_during_launch_show_only_live_files_without_rewriting_history() {
        let fixture = Fixture::new().await;
        let entries = ["owned", "drift", "missing"].map(|project| {
            managed(
                project,
                ContentKind::Mod,
                &format!("{project}.jar"),
                &format!("Historical {project}"),
            )
        });
        let mut entries = entries.to_vec();
        entries.push(
            ManifestEntry::provenance(
                CanonicalId("modrinth:pack".into()),
                ProviderId::Modrinth,
                "pack".into(),
                "v1".into(),
                None,
            )
            .unwrap(),
        );
        let raw = fixture.write_manifest(
            &entries,
            &[("mods/owned.jar", b"abc"), ("mods/drift.jar", b"xyz")],
        );
        let directories = fixture.profile.services.instances.directories();
        let launch = directories.admit(&fixture.id).unwrap();
        let endpoint = format!("/api/v1/instances/{}/content", fixture.id);
        let (status, listing) = fixture.get(&endpoint).await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert_eq!(
            listing["entries"],
            json!([{"canonical_id":"modrinth:owned","kind":"mod","provider":"modrinth",
                "project_id":"owned","version_id":"v1","filename":"owned.jar","enabled":true,
                "title":"Historical owned"}])
        );
        assert_eq!(
            listing["packs"],
            json!([{"canonical_id":"modrinth:pack","provider":"modrinth","project_id":"pack",
                "version_id":"v1","paths":[]}])
        );
        let (status, updates) = fixture.get(&format!("{endpoint}/updates")).await;
        assert_eq!((status, updates), (StatusCode::OK, json!({"updates":[]})));
        assert_eq!(
            *fixture.paths.lock().unwrap(),
            ["/v2/project/owned/version"]
        );
        assert!(directories.admit(&fixture.id).is_err());
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
    async fn content_updates_skip_unavailable_projects_and_unresolved_pins_without_mutation() {
        for (other, requested) in [
            (
                "removed",
                vec![
                    "/v2/project/removed/version",
                    "/v2/project/updateable/version",
                ],
            ),
            (
                "unavailable",
                vec![
                    "/v2/project/unavailable/version",
                    "/v2/project/updateable/version",
                ],
            ),
            (
                "dependent",
                vec![
                    "/v2/project/dependent/version",
                    "/v2/project/updateable/version",
                    "/v2/versions",
                ],
            ),
        ] {
            let fixture = Fixture::new().await;
            let projects = [other, "updateable"];
            let raw = fixture.write_managed_resource_packs(&projects);
            let before_queue = serde_json::to_value(fixture.queue().snapshot()).unwrap();
            let endpoint = format!("/api/v1/instances/{}/content", fixture.id);
            let (_, before_listing) = fixture.get(&endpoint).await;
            assert_eq!(before_listing["entries"].as_array().unwrap().len(), 2);
            let (status, updates) = fixture.get(&format!("{endpoint}/updates")).await;
            assert_eq!(status, StatusCode::OK, "{other}: {updates}");
            assert_eq!(
                updates,
                json!({"updates":[{
                    "canonical_id":"modrinth:updateable", "kind":"resource_pack",
                    "current_version_id":"v1", "latest_version_id":"updateable-v2",
                    "latest_version_number":"2.0", "title":"Installed updateable"
                }]})
            );
            let mut paths = fixture.paths.lock().unwrap().clone();
            paths.sort();
            assert_eq!(paths, requested, "{other}");
            assert_eq!(fixture.get(&endpoint).await.1, before_listing);
            assert_eq!(
                std::fs::read(fixture.game.join(MANIFEST_FILE)).unwrap(),
                raw
            );
            for project in projects {
                let pack = fixture.game.join(format!("resourcepacks/{project}.zip"));
                assert_eq!(std::fs::read(pack).unwrap(), b"abc");
                assert!(
                    !fixture
                        .game
                        .join(format!("resourcepacks/{project}-v2.zip"))
                        .exists()
                );
            }
            assert_eq!(
                serde_json::to_value(fixture.queue().snapshot()).unwrap(),
                before_queue
            );
            fixture.close().await;
        }
    }

    #[tokio::test]
    async fn install_returns_accepted_identity_before_provider_io_and_retains_failure() {
        let fixture = Fixture::new().await;
        let accepted = fixture.blocked_install().await;
        let id = accepted.started_install.unwrap().install_id;
        let status = fixture.queue().status(&id).unwrap();
        assert!(!status.done);
        assert_eq!(status.outcome, None);
        assert_eq!(
            accepted.registry_revision,
            fixture.queue().snapshot().registry_revision
        );
        fixture.gate.add_permits(1);
        assert_eq!(fixture.terminal(&id).await, InstallOutcome::Failed);
        assert!(!fixture.game.join("resourcepacks/blocked.zip").exists());
        fixture.close().await;
    }

    #[tokio::test]
    async fn uninstall_names_dependents_or_removes_the_real_file() {
        let fixture = Fixture::new().await;
        let mut dependent = managed(
            "dependent",
            ContentKind::ResourcePack,
            "dependent.zip",
            "Dependent pack",
        );
        dependent = ManifestEntry::managed(
            dependent.canonical_id().clone(),
            ProviderId::Modrinth,
            "dependent".into(),
            "v1".into(),
            ContentKind::ResourcePack,
            &FileRef {
                filename: "dependent.zip".into(),
                url: "https://cdn.modrinth.com/dependent.zip".into(),
                size: Some(3),
                sha512: Some(SHA512_ABC.into()),
                sha1: None,
                primary: true,
            },
            vec![axial_app::content::model::ContentDependency {
                project_id: Some("owned".into()),
                version_id: None,
                kind: axial_app::content::model::DependencyKind::Required,
            }],
            Some("Dependent pack".into()),
        )
        .unwrap();
        fixture.write_manifest(
            &[
                managed(
                    "owned",
                    ContentKind::ResourcePack,
                    "owned.zip",
                    "Owned pack",
                ),
                dependent,
            ],
            &[
                ("resourcepacks/owned.zip", b"abc"),
                ("resourcepacks/dependent.zip", b"abc"),
                ("resourcepacks/user.zip", b"untouched"),
            ],
        );
        let uninstall = format!("/api/v1/instances/{}/content/uninstall", fixture.id);
        let (status, refusal) = fixture
            .post(&uninstall, json!({"canonical_ids":["modrinth:owned"]}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
        assert_eq!(
            refusal["required_by"],
            json!([{"canonical_id":"modrinth:dependent","title":"Dependent pack"}])
        );
        assert!(fixture.queue().snapshot().items.is_empty());
        let before = fixture.queue().snapshot().registry_revision;
        let (status, value) = fixture
            .post(
                &uninstall,
                json!({"canonical_ids":["modrinth:owned", "modrinth:dependent"]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let accepted: InstallQueueStateResponse = serde_json::from_value(value).unwrap();
        assert_eq!(queued_label(&accepted), "Remove Owned pack and 1 more");
        let id = accepted.started_install.unwrap().install_id;
        assert_eq!(fixture.terminal(&id).await, InstallOutcome::Succeeded);
        assert!(fixture.queue().snapshot().registry_revision > before);
        assert!(!fixture.game.join("resourcepacks/owned.zip").exists());
        assert_eq!(
            std::fs::read(fixture.game.join("resourcepacks/user.zip")).unwrap(),
            b"untouched"
        );
        let manifest = std::fs::read(fixture.game.join(MANIFEST_FILE)).unwrap();
        assert!(
            ContentManifest::decode_managed(Some(&manifest))
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
                version_id: "pack-v2".into(),
                selected_file_ids: Vec::new(),
                include_overrides: true,
            }
        );
        let paths = fixture.paths.lock().unwrap().clone();
        for requested in ["/v2/project/pack", "/v2/project/pack/version"] {
            assert_eq!(paths.iter().filter(|path| *path == requested).count(), 1);
        }
        assert!(paths.iter().all(|path| !path.ends_with(".mrpack")));
        fixture
            .queue()
            .remove(&accepted.items[0].queue_id)
            .await
            .unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn invalid_content_requests_are_fixed_json_without_queue_or_provider_work() {
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
            (
                &format!("/api/v1/instances/{}/content/uninstall", fixture.id),
                json!({"canonical_ids":["modrinth:duplicate", "modrinth:duplicate"]}),
            ),
        ] {
            let (status, response) = fixture.post(path, request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        }
        for query in ["kind=secret-private-value", "kind=mod&limit=no"] {
            assert_eq!(
                fixture
                    .get(&format!("/api/v1/content/search?{query}"))
                    .await,
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error":"Invalid content query."})
                )
            );
        }
        assert!(fixture.queue().snapshot().active.is_none());
        assert!(fixture.queue().snapshot().items.is_empty());
        assert!(fixture.paths.lock().unwrap().is_empty());
        fixture.close().await;
    }
}
