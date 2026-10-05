//! HTTP adaptation only. Mount under the authenticated local transport boundary.

use axial_app::{
    accounts::model::{offline_uuid, validate_username},
    media::{MediaError, SKIN_PNG_MAX_BYTES},
    skins::{
        ProfileMedia, SkinError,
        delivery::{offline_head_svg, offline_profile},
        library::{
            CapeUpdate, ReplaceSkinOptions, SaveSkinOptions, SavedSkinDeleteResult,
            SkinLibraryError, UpdateSavedSkinRequest, normalize_upload, validate_variant,
        },
        lookup::ProfileMediaError,
    },
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Response, ApiError>;
type SkinState = Arc<ProfileMedia>;

pub fn router(service: SkinState) -> Router {
    Router::new()
        .route("/api/v1/skin/profile", get(profile))
        .route("/api/v1/skin/profile/reset", post(reset_skin))
        .route("/api/v1/skin/profile/file", get(profile_file))
        .route("/api/v1/skin/cape/file", get(cape_file))
        .route("/api/v1/skin/cape/reset", post(reset_cape))
        .route("/api/v1/skin/head", get(head))
        .route("/api/v1/skin/lookup", get(lookup))
        .route("/api/v1/skin/lookup/file", get(lookup_file))
        .route("/api/v1/skin/lookup/head", get(lookup_head))
        .route("/api/v1/skin/lookup/cape", get(lookup_cape))
        .route("/api/v1/skins/normalize", post(normalize))
        .route("/api/v1/skins", get(list).post(save))
        .route("/api/v1/skins/from-profile", post(save_profile))
        .route("/api/v1/skins/from-username", post(save_username))
        .route("/api/v1/skins/pending", get(pending).delete(cancel))
        .route("/api/v1/skins/{texture_key}", delete(remove).put(update))
        .route(
            "/api/v1/skins/{texture_key}/texture",
            axum::routing::put(replace),
        )
        .route("/api/v1/skins/{texture_key}/file", get(saved_file))
        .route("/api/v1/skins/{texture_key}/apply", post(apply))
        .route("/api/v1/skins/flush", post(flush))
        .with_state(service)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkinQuery {
    username: Option<String>,
    size: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LookupQuery {
    username: String,
    size: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFileQuery {
    texture: Option<String>,
    profile: Option<String>,
    skin: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapeQuery {
    id: String,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveQuery {
    name: Option<String>,
    variant: Option<String>,
    cape_id: Option<String>,
    source: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceQuery {
    name: Option<String>,
    variant: Option<String>,
    cape_id: Option<String>,
    clear_cape: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyQuery {
    defer: Option<bool>,
    expected_account_id: String,
    expected_selection_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingCommandQuery {
    expected_account_id: String,
    expected_selection_revision: u64,
    expected_generation: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionQuery {
    expected_account_id: String,
    expected_selection_revision: u64,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FromProfile {
    name: Option<String>,
    variant: Option<String>,
    mark_current: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FromUsername {
    username: String,
    name: Option<String>,
    variant: Option<String>,
}

fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(value)| value)
        .map_err(|_| invalid_request())
}

async fn json_body<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = to_bytes(body, 4096).await.map_err(|_| invalid_request())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_request())
}

async fn image_body(body: Body) -> Result<axum::body::Bytes, ApiError> {
    to_bytes(body, SKIN_PNG_MAX_BYTES)
        .await
        .map_err(|_| error(StatusCode::PAYLOAD_TOO_LARGE, "Skin upload is too large."))
}

async fn profile(
    State(service): State<SkinState>,
    parameters: Result<Query<SkinQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    let profile = if let Some(username) = parameters.username {
        let username = validate_username(&username).map_err(|_| invalid_request())?;
        offline_profile(&username, &offline_uuid(&username))
    } else {
        service.profile().map_err(profile_error)?
    };
    Ok(Json(profile).into_response())
}

async fn head(
    State(service): State<SkinState>,
    parameters: Result<Query<SkinQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    let profile = if let Some(username) = parameters.username {
        let username = validate_username(&username).map_err(|_| invalid_request())?;
        offline_profile(&username, &offline_uuid(&username))
    } else {
        service.profile().map_err(profile_error)?
    };
    if let Some(texture) = profile.texture_url {
        return Ok(image(
            service
                .delivery
                .head(&texture, parameters.size)
                .await
                .map_err(profile_error)?,
            false,
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        offline_head_svg(&profile.uuid, parameters.size),
    )
        .into_response())
}

async fn profile_file(
    State(service): State<SkinState>,
    parameters: Result<Query<ProfileFileQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    Ok(image(
        service
            .profile_file_for_identity(
                parameters.texture.as_deref(),
                parameters.profile.as_deref(),
                parameters.skin.as_deref(),
            )
            .await
            .map_err(profile_error)?,
        parameters.texture.is_some(),
    ))
}

async fn cape_file(
    State(service): State<SkinState>,
    parameters: Result<Query<CapeQuery>, QueryRejection>,
) -> ApiResult {
    Ok(image(
        service
            .cape_file(&query(parameters)?.id)
            .await
            .map_err(profile_error)?,
        false,
    ))
}

async fn lookup(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> ApiResult {
    Ok(Json(
        service
            .delivery
            .lookup
            .lookup(&query(parameters)?.username)
            .await
            .map_err(profile_error)?,
    )
    .into_response())
}

async fn lookup_file(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> ApiResult {
    let profile = service
        .delivery
        .lookup
        .lookup(&query(parameters)?.username)
        .await
        .map_err(profile_error)?;
    Ok(image(
        service
            .delivery
            .skin(&profile.texture_url)
            .await
            .map_err(profile_error)?,
        false,
    ))
}

async fn lookup_head(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    let profile = service
        .delivery
        .lookup
        .lookup(&parameters.username)
        .await
        .map_err(profile_error)?;
    Ok(image(
        service
            .delivery
            .head(&profile.texture_url, parameters.size)
            .await
            .map_err(profile_error)?,
        false,
    ))
}

async fn lookup_cape(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> ApiResult {
    let profile = service
        .delivery
        .lookup
        .lookup(&query(parameters)?.username)
        .await
        .map_err(profile_error)?;
    let url = profile
        .cape_url
        .ok_or_else(|| profile_error(ProfileMediaError::MissingCape))?;
    Ok(image(
        service.delivery.cape(&url).await.map_err(profile_error)?,
        false,
    ))
}

async fn normalize(body: Body) -> ApiResult {
    Ok(Json(normalize_upload(&image_body(body).await?).map_err(library_error)?).into_response())
}

async fn list(State(service): State<SkinState>) -> ApiResult {
    Ok(Json(service.list().map_err(skin_error)?).into_response())
}
async fn pending(State(service): State<SkinState>) -> ApiResult {
    Ok(Json(service.pending_status().map_err(profile_error)?).into_response())
}

async fn save(
    State(service): State<SkinState>,
    parameters: Result<Query<SaveQuery>, QueryRejection>,
    body: Body,
) -> ApiResult {
    let parameters = query(parameters)?;
    let variant = parameters
        .variant
        .as_deref()
        .map(validate_variant)
        .transpose()
        .map_err(library_error)?;
    let record = service
        .save_upload(
            &image_body(body).await?,
            SaveSkinOptions {
                name: parameters.name.unwrap_or_default(),
                variant,
                cape_id: parameters.cape_id,
            },
            parameters.source.as_deref(),
        )
        .await
        .map_err(skin_error)?;
    Ok(Json(record).into_response())
}

async fn save_profile(
    State(service): State<SkinState>,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
    body: Body,
) -> ApiResult {
    let parameters = query(parameters)?;
    let request: FromProfile = json_body(body).await?;
    let variant = request
        .variant
        .as_deref()
        .map(validate_variant)
        .transpose()
        .map_err(library_error)?;
    Ok(Json(
        service
            .save_profile(
                request.name,
                variant,
                request.mark_current.unwrap_or(false),
                &parameters.expected_account_id,
                parameters.expected_selection_revision,
            )
            .await
            .map_err(skin_error)?,
    )
    .into_response())
}

async fn save_username(State(service): State<SkinState>, body: Body) -> ApiResult {
    let request: FromUsername = json_body(body).await?;
    let variant = request
        .variant
        .as_deref()
        .map(validate_variant)
        .transpose()
        .map_err(library_error)?;
    Ok(Json(
        service
            .save_username(&request.username, request.name, variant)
            .await
            .map_err(skin_error)?,
    )
    .into_response())
}

async fn remove(State(service): State<SkinState>, Path(key): Path<String>) -> ApiResult {
    match service.delete_saved(&key).await.map_err(skin_error)? {
        SavedSkinDeleteResult::Deleted(_) => Ok(Json(json!({"status":"deleted"})).into_response()),
        SavedSkinDeleteResult::Missing => Err(profile_error(ProfileMediaError::SavedSkinMissing)),
        SavedSkinDeleteResult::Applied => Err(error(
            StatusCode::CONFLICT,
            "Reset or apply another skin before deleting the applied saved skin.",
        )),
    }
}

async fn update(
    State(service): State<SkinState>,
    Path(key): Path<String>,
    body: Body,
) -> ApiResult {
    let request: UpdateSavedSkinRequest = json_body(body).await?;
    let record = service
        .update_saved(&key, request)
        .await
        .map_err(skin_error)?
        .ok_or_else(|| profile_error(ProfileMediaError::SavedSkinMissing))?;
    Ok(Json(record).into_response())
}

async fn replace(
    State(service): State<SkinState>,
    Path(key): Path<String>,
    parameters: Result<Query<ReplaceQuery>, QueryRejection>,
    body: Body,
) -> ApiResult {
    let parameters = query(parameters)?;
    if parameters.clear_cape == Some(true) && parameters.cape_id.is_some() {
        return Err(invalid_request());
    }
    let cape_id = if parameters.clear_cape == Some(true) {
        CapeUpdate::Clear
    } else {
        parameters.cape_id.map(CapeUpdate::Set).unwrap_or_default()
    };
    let variant = parameters
        .variant
        .as_deref()
        .map(validate_variant)
        .transpose()
        .map_err(library_error)?;
    let record = service
        .replace_saved(
            &key,
            &image_body(body).await?,
            ReplaceSkinOptions {
                name: parameters.name,
                variant,
                cape_id,
            },
        )
        .await
        .map_err(skin_error)?
        .ok_or_else(|| profile_error(ProfileMediaError::SavedSkinMissing))?;
    Ok(Json(record).into_response())
}

async fn saved_file(State(service): State<SkinState>, Path(key): Path<String>) -> ApiResult {
    let png = service
        .library
        .read_png(&key)
        .map_err(library_error)?
        .ok_or_else(|| profile_error(ProfileMediaError::SavedSkinMissing))?;
    Ok(image(png, true))
}

async fn apply(
    State(service): State<SkinState>,
    Path(key): Path<String>,
    parameters: Result<Query<ApplyQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    let deferred = parameters.defer.unwrap_or(false);
    let accepted = service
        .queue(
            &key,
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
        )
        .map_err(skin_error)?;
    if !deferred {
        service
            .flush_generation(&accepted.account_id, accepted.generation)
            .await
            .map_err(profile_error)?;
    }
    Ok(Json(json!({ "status": if deferred { "queued" } else { "applied" }, "texture_key": key,
        "pending": if deferred { Some(accepted) } else { None },
        "profile_updated": !deferred, "view_model": { "summary": if deferred { "Skin queued." } else { "Skin applied." } } })).into_response())
}

async fn flush(
    State(service): State<SkinState>,
    parameters: Result<Query<PendingCommandQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    let applied = service
        .flush_selected(
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
            parameters.expected_generation,
        )
        .await
        .map_err(profile_error)?;
    Ok(Json(json!({"status":"flushed","applied":applied,"view_model":{"summary":if applied > 0 {"Skin applied."} else {"No skin change queued."}}})).into_response())
}

async fn cancel(
    State(service): State<SkinState>,
    parameters: Result<Query<PendingCommandQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    let cleared = service
        .cancel_selected(
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
            parameters.expected_generation,
        )
        .map_err(profile_error)?;
    Ok(Json(json!({"status":"cleared","cleared":cleared,"view_model":{"summary":"Skin change canceled."}})).into_response())
}

async fn reset_skin(
    State(service): State<SkinState>,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    service
        .reset(
            true,
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
        )
        .await
        .map_err(profile_error)?;
    Ok(Json(json!({"status":"reset","profile_updated":true,"view_model":{"summary":"Profile skin reset."}})).into_response())
}

async fn reset_cape(
    State(service): State<SkinState>,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
) -> ApiResult {
    let parameters = query(parameters)?;
    service
        .reset(
            false,
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
        )
        .await
        .map_err(profile_error)?;
    Ok(Json(
        json!({"status":"reset","profile_updated":true,"view_model":{"summary":"Cape reset."}}),
    )
    .into_response())
}

fn image(bytes: Vec<u8>, immutable: bool) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CACHE_CONTROL,
                if immutable {
                    "private, max-age=31536000, immutable"
                } else {
                    "private, no-store"
                },
            ),
        ],
        bytes,
    )
        .into_response()
}

fn invalid_request() -> ApiError {
    error(StatusCode::BAD_REQUEST, "Invalid skin request.")
}
fn error(status: StatusCode, message: impl ToString) -> ApiError {
    (status, Json(json!({"error":message.to_string()})))
}
fn skin_error(value: SkinError) -> ApiError {
    match value {
        SkinError::Library(value) => library_error(value),
        SkinError::Profile(value) => profile_error(value),
    }
}
pub(super) fn library_error(value: SkinLibraryError) -> ApiError {
    let status = match value {
        SkinLibraryError::Conflict => StatusCode::CONFLICT,
        SkinLibraryError::Image(MediaError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
        SkinLibraryError::Capacity | SkinLibraryError::StorageFull => {
            StatusCode::INSUFFICIENT_STORAGE
        }
        SkinLibraryError::Storage
        | SkinLibraryError::PermissionDenied
        | SkinLibraryError::InvalidData => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    error(status, value)
}
fn profile_error(value: ProfileMediaError) -> ApiError {
    let status = match value {
        ProfileMediaError::InvalidUsername
        | ProfileMediaError::InvalidTexture
        | ProfileMediaError::InvalidImage
        | ProfileMediaError::Rejected => StatusCode::BAD_REQUEST,
        ProfileMediaError::AccountRequired => StatusCode::UNAUTHORIZED,
        ProfileMediaError::OwnershipMissing => StatusCode::CONFLICT,
        ProfileMediaError::NotFound
        | ProfileMediaError::MissingSkin
        | ProfileMediaError::MissingCape
        | ProfileMediaError::SavedSkinMissing => StatusCode::NOT_FOUND,
        ProfileMediaError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ProfileMediaError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ProfileMediaError::StaleIdentity
        | ProfileMediaError::StaleIntent
        | ProfileMediaError::Cancelled
        | ProfileMediaError::Busy => StatusCode::CONFLICT,
        ProfileMediaError::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    error(status, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        accounts::{
            credential_store::CredentialStore, directory::AccountDirectory,
            microsoft::MinecraftProfile, model::MicrosoftIdentity, selection::CapturedAccount,
            session::AuthService,
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        skins::{library::SavedSkinLibrary, store::SavedSkinStore},
        storage::MetadataStore,
        tasks::TaskOwner,
    };
    use axum::http::{Method, Request};
    use std::time::Duration;
    use tower::ServiceExt;

    struct Fixture {
        _root: tempfile::TempDir,
        accounts: Arc<AccountDirectory>,
        service: SkinState,
        app: Router,
        key: String,
        tasks: TaskOwner,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let roots = match LibraryLifecycle::open(&directory.path().canonicalize().unwrap()) {
                LibraryOpenOutcome::Ready(roots) => roots,
                _ => panic!("isolated skin root"),
            };
            let root = roots.admit_application_root().unwrap();
            let metadata = Arc::new(MetadataStore::in_memory().unwrap());
            let accounts = Arc::new(AccountDirectory::new(metadata.clone()).unwrap());
            metadata
                .migrate(&[axial_app::skins::store::MIGRATION])
                .unwrap();
            let tasks = TaskOwner::new(16).unwrap();
            let auth = Arc::new(AuthService::new(
                accounts.clone(),
                Arc::new(CredentialStore::isolated_for_tests()),
                tasks.clone(),
            ));
            let library = Arc::new(SavedSkinLibrary::new(
                SavedSkinStore::new(metadata),
                root.clone(),
            ));
            let mut png = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut png, 64, 64);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&[11, 22, 33, 255].repeat(64 * 64))
                    .unwrap();
            }
            let saved = library
                .save_upload(
                    &png,
                    SaveSkinOptions {
                        name: "Fixture skin".into(),
                        variant: None,
                        cape_id: None,
                    },
                    None,
                )
                .unwrap();
            let service =
                ProfileMedia::new(library, accounts.clone(), auth, tasks.clone(), root).unwrap();
            Self {
                _root: directory,
                accounts,
                app: router(service.clone()),
                service,
                key: saved.texture_key,
                tasks,
            }
        }

        fn add_account(&self, profile_id: &str, name: &str) -> CapturedAccount {
            self.accounts
                .commit_microsoft(
                    self.accounts.selection_revision().unwrap(),
                    MicrosoftIdentity {
                        login_id: uuid::Uuid::new_v4().to_string(),
                        profile_id: profile_id.into(),
                        display_name: name.into(),
                        credential_revision: 1,
                        owns_minecraft_java: true,
                        profile: MinecraftProfile {
                            id: profile_id.into(),
                            name: name.into(),
                            skins: Vec::new(),
                            capes: Vec::new(),
                        },
                    },
                )
                .unwrap()
        }

        async fn request(&self, method: Method, path: &str) -> (StatusCode, Value) {
            self.request_body(method, path, Body::empty()).await
        }

        async fn request_body(
            &self,
            method: Method,
            path: &str,
            body: Body,
        ) -> (StatusCode, Value) {
            let response = self
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        }

        fn apply_path(&self, capture: &CapturedAccount) -> String {
            format!(
                "/api/v1/skins/{}/apply?defer=true&{}",
                self.key,
                selection_query(capture)
            )
        }

        async fn queue(&self, capture: &CapturedAccount) -> u64 {
            let (status, body) = self.request(Method::POST, &self.apply_path(capture)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["pending"]["account_id"], capture.account_id());
            assert_eq!(body["pending"]["texture_key"], self.key);
            body["pending"]["generation"].as_u64().unwrap()
        }

        async fn finish(&self) {
            for account in self.accounts.snapshot().unwrap().accounts {
                self.service.account_removed(account.account_id.as_str());
            }
            tokio::time::timeout(Duration::from_secs(2), self.service.shutdown())
                .await
                .unwrap()
                .unwrap();
            self.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
        }
    }

    fn selection_query(capture: &CapturedAccount) -> String {
        format!(
            "expected_account_id={}&expected_selection_revision={}",
            capture.account_id(),
            capture.selection_revision()
        )
    }

    fn pending_path(route: &str, capture: &CapturedAccount, generation: u64) -> String {
        format!(
            "/api/v1/skins/{route}?{}&expected_generation={generation}",
            selection_query(capture)
        )
    }

    #[tokio::test]
    async fn delayed_account_commands_cannot_affect_the_new_selection_or_a_reselected_account() {
        let fixture = Fixture::new();
        let first = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let second = fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
        fixture.accounts.select(first.account_id()).unwrap();
        let first = fixture.accounts.capture_selected().unwrap();
        let first_generation = fixture.queue(&first).await;
        // These requests were built by A's view but have not reached the route.
        let delayed = [
            (Method::POST, fixture.apply_path(&first)),
            (
                Method::POST,
                pending_path("flush", &first, first_generation),
            ),
            (
                Method::DELETE,
                pending_path("pending", &first, first_generation),
            ),
        ];
        fixture.accounts.select(second.account_id()).unwrap();
        let second = fixture.accounts.capture_selected().unwrap();
        fixture.queue(&second).await;
        let before = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        for (method, path) in &delayed {
            let (status, body) = fixture.request(method.clone(), path).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .contains("reload the account")
            );
            assert_eq!(
                fixture.request(Method::GET, "/api/v1/skins/pending").await,
                before
            );
        }
        fixture.accounts.select(first.account_id()).unwrap();
        let before = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        assert_eq!(before.1["generation"], first_generation);
        for (method, path) in &delayed {
            assert_eq!(
                fixture.request(method.clone(), path).await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                fixture.request(Method::GET, "/api/v1/skins/pending").await,
                before
            );
        }
        fixture.finish().await;
    }

    async fn assert_stale_reset_preserves_pending(route: &str, reselected: bool) {
        let fixture = Fixture::new();
        let first = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let second = fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
        fixture.accounts.select(first.account_id()).unwrap();
        let confirmed = fixture.accounts.capture_selected().unwrap();
        let delayed = format!("{route}?{}", selection_query(&confirmed));
        fixture.accounts.select(second.account_id()).unwrap();
        if reselected {
            fixture.accounts.select(first.account_id()).unwrap();
        }
        let current = fixture.accounts.capture_selected().unwrap();
        fixture.queue(&current).await;
        let before = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        assert_eq!(before.1["phase"], "queued");
        let (status, body) = fixture.request(Method::POST, &delayed).await;
        let after = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        fixture.finish().await;
        assert_eq!(
            after, before,
            "stale reset must not cancel the current choice"
        );
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn stale_profile_resets_cannot_cancel_another_accounts_pending_skin() {
        for route in ["/api/v1/skin/profile/reset", "/api/v1/skin/cape/reset"] {
            assert_stale_reset_preserves_pending(route, false).await;
        }
    }

    #[tokio::test]
    async fn stale_profile_resets_cannot_cancel_a_reselected_accounts_pending_skin() {
        for route in ["/api/v1/skin/profile/reset", "/api/v1/skin/cape/reset"] {
            assert_stale_reset_preserves_pending(route, true).await;
        }
    }

    #[tokio::test]
    async fn profile_reset_requires_a_complete_valid_selection_fence_before_cancelling() {
        let fixture = Fixture::new();
        let capture = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        fixture.queue(&capture).await;
        let before = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        for route in ["/api/v1/skin/profile/reset", "/api/v1/skin/cape/reset"] {
            for suffix in [
                String::new(),
                format!("?expected_account_id={}", capture.account_id()),
                format!(
                    "?expected_selection_revision={}",
                    capture.selection_revision()
                ),
                format!(
                    "?expected_account_id={}&expected_selection_revision=invalid",
                    capture.account_id()
                ),
                format!(
                    "?expected_account_id={}&expected_selection_revision=18446744073709551616",
                    capture.account_id()
                ),
            ] {
                let (status, _) = fixture
                    .request(Method::POST, &format!("{route}{suffix}"))
                    .await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(
                    fixture.request(Method::GET, "/api/v1/skins/pending").await,
                    before
                );
            }
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn current_profile_reset_only_cancels_its_captured_pending_skin() {
        for route in ["/api/v1/skin/profile/reset", "/api/v1/skin/cape/reset"] {
            let fixture = Fixture::new();
            let first = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
            fixture.queue(&first).await;
            let second = fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
            fixture.queue(&second).await;
            let other = fixture.request(Method::GET, "/api/v1/skins/pending").await;
            fixture.accounts.select(first.account_id()).unwrap();
            let current = fixture.accounts.capture_selected().unwrap();
            let path = format!("{route}?{}", selection_query(&current));
            let (status, _) = fixture.request(Method::POST, &path).await;
            let after = fixture.request(Method::GET, "/api/v1/skins/pending").await;
            fixture.accounts.select(second.account_id()).unwrap();
            let other_after = fixture.request(Method::GET, "/api/v1/skins/pending").await;
            fixture.finish().await;
            // Admission cancels only its target before the absent fixture credentials refuse I/O.
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(after.1["phase"], "idle");
            assert_eq!(other_after, other);
        }
    }

    #[tokio::test]
    async fn profile_save_rejects_stale_selection_before_profile_access() {
        let fixture = Fixture::new();
        let confirmed = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
        let path = format!("/api/v1/skins/from-profile?{}", selection_query(&confirmed));
        for reselected in [false, true] {
            if reselected {
                fixture.accounts.select(confirmed.account_id()).unwrap();
            }
            let before = fixture.request(Method::GET, "/api/v1/skins").await;
            let (status, body) = fixture
                .request_body(
                    Method::POST,
                    &path,
                    Body::from(json!({"variant":"classic","mark_current":true}).to_string()),
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"], ProfileMediaError::StaleIdentity.to_string());
            assert_eq!(fixture.request(Method::GET, "/api/v1/skins").await, before);
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn profile_save_requires_a_complete_valid_selection_fence() {
        let fixture = Fixture::new();
        let capture = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let before = fixture.request(Method::GET, "/api/v1/skins").await;
        for suffix in [
            String::new(),
            format!("?expected_account_id={}", capture.account_id()),
            format!(
                "?expected_selection_revision={}",
                capture.selection_revision()
            ),
            format!(
                "?expected_account_id={}&expected_selection_revision=invalid",
                capture.account_id()
            ),
            format!(
                "?expected_account_id={}&expected_selection_revision=18446744073709551616",
                capture.account_id()
            ),
        ] {
            let (status, _) = fixture
                .request_body(
                    Method::POST,
                    &format!("/api/v1/skins/from-profile{suffix}"),
                    Body::from(json!({"variant":"classic","mark_current":true}).to_string()),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(fixture.request(Method::GET, "/api/v1/skins").await, before);
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn stale_pending_generation_cannot_cancel_or_flush_a_newer_choice() {
        let fixture = Fixture::new();
        let capture = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let old_generation = fixture.queue(&capture).await;
        let current_generation = fixture.queue(&capture).await;
        assert!(current_generation > old_generation);
        let before = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        for (method, route) in [(Method::DELETE, "pending"), (Method::POST, "flush")] {
            let (status, body) = fixture
                .request(method, &pending_path(route, &capture, old_generation))
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .contains("reload the wardrobe")
            );
            assert_eq!(
                fixture.request(Method::GET, "/api/v1/skins/pending").await,
                before
            );
        }
        let (status, body) = fixture
            .request(
                Method::DELETE,
                &pending_path("pending", &capture, current_generation),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cleared"], true);
        let (_, after) = fixture.request(Method::GET, "/api/v1/skins/pending").await;
        assert_eq!(after["phase"], "idle");
        assert!(after["texture_key"].is_null());
        fixture.finish().await;
    }

    #[tokio::test]
    async fn account_mutations_require_explicit_command_preconditions() {
        let fixture = Fixture::new();
        let capture = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        for (method, path) in [
            (
                Method::POST,
                format!("/api/v1/skins/{}/apply?defer=true", fixture.key),
            ),
            (Method::POST, "/api/v1/skins/flush".into()),
            (Method::DELETE, "/api/v1/skins/pending".into()),
            (
                Method::DELETE,
                format!("/api/v1/skins/pending?{}", selection_query(&capture)),
            ),
        ] {
            assert_eq!(
                fixture.request(method, &path).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        assert!(
            fixture
                .request(Method::GET, "/api/v1/skins/pending")
                .await
                .1
                .is_null()
        );
        fixture.finish().await;
    }
}
