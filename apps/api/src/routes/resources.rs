//! Resource selectors are resolved only through registered instance authority.

use super::{
    error::{ApiError, ApiResult, Extracted},
    media,
};
use axial_app::{
    instances::model::InstanceId,
    resources::{
        ResourceCommand, ResourceError, ResourceService,
        folders::{FolderError, FolderService},
        mods::{MOD_IMPORT_MAX_BYTES, ModImportError, ModImportResponse, UpdateModRequest},
        packs::PackFolder,
        screenshots::RenameScreenshotRequest,
        worlds::RenameWorldRequest,
    },
    tasks::TaskHandle,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    response::Response,
    routing::{MethodFilter, MethodRouter, get, on, post, put},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

type Resources = Arc<ResourceService>;
type Command = Result<TaskHandle<Result<ResourceCommand, ResourceError>>, ResourceError>;

pub fn router(state: Resources) -> Router {
    Router::new()
        .route("/api/v1/instances/{id}/resources", listing(ResourceService::resources))
        .route("/api/v1/instances/{id}/mods", listing(ResourceService::mods))
        .route(
            "/api/v1/instances/{id}/mods/metadata",
            listing(ResourceService::mod_metadata),
        )
        .route(
            "/api/v1/instances/{id}/mods/{name}/icon",
            file(|state, id, name| Ok(("image/png", state.mod_icon(id, name)?))),
        )
        .route(
            "/api/v1/instances/{id}/resourcepacks",
            listing(|state, id| state.packs(id, PackFolder::ResourcePacks)),
        )
        .route(
            "/api/v1/instances/{id}/resourcepacks/{name}",
            named_command(MethodFilter::DELETE, |state, id, name| {
                state.delete_pack(id, PackFolder::ResourcePacks, name)
            }),
        )
        .route(
            "/api/v1/instances/{id}/resourcepacks/{name}/icon",
            file(|state, id, name| Ok(("image/png", state.pack_icon(id, name)?))),
        )
        .route(
            "/api/v1/instances/{id}/shaderpacks",
            listing(|state, id| state.packs(id, PackFolder::ShaderPacks)),
        )
        .route(
            "/api/v1/instances/{id}/shaderpacks/{name}",
            named_command(MethodFilter::DELETE, |state, id, name| {
                state.delete_pack(id, PackFolder::ShaderPacks, name)
            }),
        )
        .route("/api/v1/instances/{id}/worlds", listing(ResourceService::worlds))
        .route(
            "/api/v1/instances/{id}/screenshots",
            listing(ResourceService::screenshots),
        )
        .route("/api/v1/instances/{id}/logs", listing(ResourceService::logs))
        .route(
            "/api/v1/instances/{id}/logs/{name}",
            get(
                |State(state): State<Resources>, Path((raw, name)): Path<(String, String)>| async move {
                    blocking(state, &raw, move |state, id| state.log(id, &name))
                        .await
                        .map(Json)
                },
            ),
        )
        .route(
            "/api/v1/instances/{id}/mods/{name}",
            put(toggle_mod).merge(named_command(
                MethodFilter::DELETE,
                ResourceService::delete_mod,
            )),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}",
            put(rename_screenshot).merge(named_command(
                MethodFilter::DELETE,
                ResourceService::delete_screenshot,
            )),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}/file",
            file(|state, id, name| {
                let media = state.screenshot(id, name)?;
                Ok((media.content_type, media.bytes))
            }),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}",
            put(rename_world).merge(named_command(
                MethodFilter::DELETE,
                ResourceService::delete_world,
            )),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}/backup",
            named_command(MethodFilter::POST, ResourceService::backup_world),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}/icon",
            file(|state, id, name| {
                let icon = state.world_icon(id, name)?;
                Ok((icon.content_type, icon.bytes))
            }),
        )
        .layer(DefaultBodyLimit::max(4096))
        // The jar body is bounded by the handler's own read limit.
        .route("/api/v1/instances/{id}/mods/import", post(import_mod))
        .with_state(state)
}

fn id(raw: &str) -> Result<InstanceId, ResourceError> {
    raw.parse().map_err(|_| ResourceError::InvalidName)
}

/// Filesystem reads run off the async workers.
async fn blocking<T: Send + 'static>(
    state: Resources,
    raw: &str,
    read: impl FnOnce(&ResourceService, &InstanceId) -> Result<T, ResourceError> + Send + 'static,
) -> Result<T, ApiError> {
    let id = id(raw)?;
    Ok(tokio::task::spawn_blocking(move || read(&state, &id))
        .await
        .map_err(|_| ResourceError::Files)??)
}

fn listing<T: Serialize + Send + 'static>(
    read: fn(&ResourceService, &InstanceId) -> Result<T, ResourceError>,
) -> MethodRouter<Resources> {
    get(
        move |State(state): State<Resources>, Path(raw): Path<String>| async move {
            blocking(state, &raw, read).await.map(Json)
        },
    )
}

type FileRead =
    fn(&ResourceService, &InstanceId, &str) -> Result<(&'static str, Vec<u8>), ResourceError>;

fn file(read: FileRead) -> MethodRouter<Resources> {
    get(
        move |State(state): State<Resources>, Path((raw, name)): Path<(String, String)>| async move {
            let (content_type, bytes) =
                blocking(state, &raw, move |state, id| read(state, id, &name)).await?;
            Ok::<Response, ApiError>(media(content_type, "no-store", bytes))
        },
    )
}

async fn command(work: Command) -> ApiResult<ResourceCommand> {
    Ok(Json(
        work?.join().await.map_err(|_| ResourceError::Pending)??,
    ))
}

fn named_command(
    method: MethodFilter,
    run: fn(&ResourceService, &InstanceId, &str) -> Command,
) -> MethodRouter<Resources> {
    on(
        method,
        move |State(state): State<Resources>, Path((raw, name)): Path<(String, String)>| async move {
            command(run(&state, &id(&raw)?, &name)).await
        },
    )
}

async fn toggle_mod(
    State(state): State<Resources>,
    Path((raw, name)): Path<(String, String)>,
    request: Result<Json<UpdateModRequest>, JsonRejection>,
) -> ApiResult<ResourceCommand> {
    let request = request.or_refuse(|| ResourceError::InvalidName)?;
    command(state.set_mod_enabled(&id(&raw)?, &name, request.enabled, request.with_dependents))
        .await
}

async fn rename_screenshot(
    State(state): State<Resources>,
    Path((raw, name)): Path<(String, String)>,
    request: Result<Json<RenameScreenshotRequest>, JsonRejection>,
) -> ApiResult<ResourceCommand> {
    let to = request.or_refuse(|| ResourceError::InvalidName)?.name;
    command(state.rename_screenshot(&id(&raw)?, &name, &to)).await
}

async fn rename_world(
    State(state): State<Resources>,
    Path((raw, name)): Path<(String, String)>,
    request: Result<Json<RenameWorldRequest>, JsonRejection>,
) -> ApiResult<ResourceCommand> {
    let to = request.or_refuse(|| ResourceError::InvalidName)?.name;
    command(state.rename_world(&id(&raw)?, &name, &to)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportQuery {
    name: String,
}

/// One jar per request: the body is the file and `name` its filename.
async fn import_mod(
    State(state): State<Resources>,
    Path(raw): Path<String>,
    query: Result<Query<ImportQuery>, QueryRejection>,
    body: axum::body::Body,
) -> ApiResult<ModImportResponse> {
    let id = id(&raw)?;
    let query = query.or_refuse(|| ModImportError::NotJar)?;
    let bytes = axum::body::to_bytes(body, MOD_IMPORT_MAX_BYTES)
        .await
        .map_err(|_| ModImportError::TooLarge(query.name.chars().take(255).collect()))?;
    let work = state.import_mods(&id, vec![(query.name, bytes.to_vec())])?;
    Ok(Json(
        work.join().await.map_err(|_| ModImportError::Pending)??,
    ))
}

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

async fn open_folder(
    State(state): State<Arc<FolderService>>,
    Path(raw): Path<String>,
    query: Result<Query<FolderQuery>, QueryRejection>,
) -> ApiResult<super::Status> {
    let query = query.or_refuse(|| FolderError::InvalidFolder)?;
    state.open(&id(&raw)?, query.sub.as_deref())?.wait().await?;
    Ok(Json(super::Status::OK))
}

#[cfg(test)]
mod tests {
    use crate::fixture::{Api, Profile};
    use axial_app::instances::model::InstanceId;
    use reqwest::{Method, StatusCode};
    use serde_json::{Value, json};
    use std::io::Write;

    async fn import(
        profile: &Profile,
        id: &InstanceId,
        name: &str,
        bytes: Vec<u8>,
    ) -> (StatusCode, Value) {
        let path = format!("/api/v1/instances/{id}/mods/import?name={name}");
        Api::json(profile.api.builder(Method::POST, &path).body(bytes)).await
    }

    #[tokio::test]
    async fn mod_import_adds_a_jar_above_the_json_body_limit_and_names_refusals() {
        let profile = Profile::start().await;
        let (id, game) = profile.create_instance().await;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        // Larger than the router's 4 KiB JSON limit; the handler owns this bound.
        zip.write_all(&vec![7; 64 * 1024]).unwrap();
        let jar = zip.finish().unwrap().into_inner();
        assert_eq!(
            import(&profile, &id, "local.jar", jar.clone()).await,
            (StatusCode::OK, json!({"imported":["local.jar"]}))
        );
        assert_eq!(std::fs::read(game.join("mods/local.jar")).unwrap(), jar);
        for (name, bytes, status, error) in [
            (
                "again.jar",
                jar.clone(),
                StatusCode::CONFLICT,
                "again.jar is already in this instance.",
            ),
            (
                "notes.txt",
                jar,
                StatusCode::BAD_REQUEST,
                "Only .jar files can be added as mods.",
            ),
            (
                "broken.jar",
                b"plain text".to_vec(),
                StatusCode::BAD_REQUEST,
                "broken.jar is not a readable mod archive.",
            ),
        ] {
            assert_eq!(
                import(&profile, &id, name, bytes).await,
                (status, json!({"error":error}))
            );
        }
        let (_, listed) = profile
            .api
            .read(&format!("/api/v1/instances/{id}/mods"))
            .await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert!(!game.join("mods/again.jar").exists());
        profile.close().await;
    }

    #[tokio::test]
    async fn folder_refusals_are_fixed_json_before_any_effect() {
        let profile = Profile::start().await;
        let (id, game) = profile.create_instance().await;
        let before = std::fs::read_dir(&game).unwrap().count();
        let invalid_folder = (
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid instance folder"}),
        );
        let mut cases = [
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
        ]
        .map(|suffix| (id.to_string(), suffix, invalid_folder.clone()))
        .to_vec();
        cases.push((
            InstanceId::new().to_string(),
            "?sub=logs",
            (StatusCode::NOT_FOUND, json!({"error":"instance not found"})),
        ));
        cases.push((
            "missing".into(),
            "",
            (
                StatusCode::BAD_REQUEST,
                json!({"error":"invalid resource name or file type"}),
            ),
        ));
        for (target, suffix, expected) in cases {
            let path = format!("/api/v1/instances/{target}/open-folder{suffix}");
            assert_eq!(
                profile.api.response(Method::POST, &path, None).await,
                expected,
                "{path}"
            );
        }
        assert_eq!(std::fs::read_dir(&game).unwrap().count(), before);
        assert!(!game.parent().unwrap().join("outside").exists());
        profile.close().await;
    }

    #[tokio::test]
    async fn media_routes_serve_unsniffed_uncached_files() {
        let profile = Profile::start().await;
        let (id, game) = profile.create_instance().await;
        std::fs::create_dir_all(game.join("screenshots")).unwrap();
        let png = b"\x89PNG\r\n\x1a\nfixture".to_vec();
        std::fs::write(game.join("screenshots/shot.png"), &png).unwrap();
        let response = profile
            .api
            .builder(
                Method::GET,
                &format!("/api/v1/instances/{id}/screenshots/shot.png/file"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for (header, value) in [
            ("content-type", "image/png"),
            ("x-content-type-options", "nosniff"),
            ("cache-control", "no-store"),
        ] {
            assert_eq!(response.headers()[header], value);
        }
        assert_eq!(response.bytes().await.unwrap(), png);
        profile.close().await;
    }
}
