//! HTTP adaptation only. Mount under the authenticated local transport boundary.

use super::{
    Status,
    error::{ApiError, ApiResult, Extracted},
    media,
};
use axial_app::{
    media::SKIN_PNG_MAX_BYTES,
    skins::{
        ProfileMedia,
        library::{
            CapeUpdate, ReplaceSkinOptions, SaveSkinOptions, SavedSkinDeleteResult,
            SavedSkinRecord, SkinNormalizeResponse, UpdateSavedSkinRequest, normalize_upload,
            validate_variant,
        },
        lookup::{ProfileMediaError, UsernameProfile},
        profile_change::SavedSkinsResponse,
        view::{ProfileResetResponse, SkinApplyResponse, SkinCancelResponse, SkinFlushResponse},
    },
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::Response,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, de::DeserializeOwned};
use std::sync::Arc;

type Media = Result<Response, ApiError>;
type SkinState = Arc<ProfileMedia>;

pub fn router(service: SkinState) -> Router {
    Router::new()
        .route("/api/v1/skin/profile/reset", post(reset_skin))
        .route("/api/v1/skin/profile/file", get(profile_file))
        .route("/api/v1/skin/cape/file", get(cape_file))
        .route("/api/v1/skin/cape/reset", post(reset_cape))
        .route("/api/v1/skin/lookup", get(lookup))
        .route("/api/v1/skin/lookup/file", get(lookup_file))
        .route("/api/v1/skin/lookup/cape", get(lookup_cape))
        .route("/api/v1/skins/normalize", post(normalize))
        .route("/api/v1/skins", get(list).post(save))
        .route("/api/v1/skins/from-profile", post(save_profile))
        .route("/api/v1/skins/from-username", post(save_username))
        .route("/api/v1/skins/pending", delete(cancel))
        .route("/api/v1/skins/{texture_key}", delete(remove).put(update))
        .route("/api/v1/skins/{texture_key}/texture", put(replace))
        .route("/api/v1/skins/{texture_key}/file", get(saved_file))
        .route("/api/v1/skins/{texture_key}/apply", post(apply))
        .route("/api/v1/skins/flush", post(flush))
        .with_state(service)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LookupQuery {
    username: String,
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

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid skin request.")
}

fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.or_refuse(invalid_request)
}

async fn json_body<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = to_bytes(body, 4096).await.map_err(|_| invalid_request())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_request())
}

async fn image_body(body: Body) -> Result<axum::body::Bytes, ApiError> {
    to_bytes(body, SKIN_PNG_MAX_BYTES)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Skin upload is too large."))
}

fn variant(value: Option<&str>) -> Result<Option<axial_app::media::SkinVariant>, ApiError> {
    Ok(value.map(validate_variant).transpose()?)
}

fn image(bytes: Vec<u8>, immutable: bool) -> Response {
    let cache = if immutable {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-store"
    };
    media("image/png", cache, bytes)
}

async fn profile_file(
    State(service): State<SkinState>,
    parameters: Result<Query<ProfileFileQuery>, QueryRejection>,
) -> Media {
    let parameters = query(parameters)?;
    let png = service
        .profile_file_for_identity(
            parameters.texture.as_deref(),
            parameters.profile.as_deref(),
            parameters.skin.as_deref(),
        )
        .await?;
    Ok(image(png, parameters.texture.is_some()))
}

async fn cape_file(
    State(service): State<SkinState>,
    parameters: Result<Query<CapeQuery>, QueryRejection>,
) -> Media {
    Ok(image(
        service.cape_file(&query(parameters)?.id).await?,
        false,
    ))
}

async fn lookup(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> ApiResult<UsernameProfile> {
    let profile = service
        .delivery
        .lookup
        .lookup(&query(parameters)?.username)
        .await?;
    Ok(Json(profile))
}

async fn lookup_file(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> Media {
    let profile = service
        .delivery
        .lookup
        .lookup(&query(parameters)?.username)
        .await?;
    Ok(image(
        service.delivery.skin(&profile.texture_url).await?,
        false,
    ))
}

async fn lookup_cape(
    State(service): State<SkinState>,
    parameters: Result<Query<LookupQuery>, QueryRejection>,
) -> Media {
    let profile = service
        .delivery
        .lookup
        .lookup(&query(parameters)?.username)
        .await?;
    let url = profile.cape_url.ok_or(ProfileMediaError::MissingCape)?;
    Ok(image(service.delivery.cape(&url).await?, false))
}

async fn normalize(body: Body) -> ApiResult<SkinNormalizeResponse> {
    Ok(Json(normalize_upload(&image_body(body).await?)?))
}

async fn list(State(service): State<SkinState>) -> ApiResult<SavedSkinsResponse> {
    Ok(Json(service.list()?))
}

async fn save(
    State(service): State<SkinState>,
    parameters: Result<Query<SaveQuery>, QueryRejection>,
    body: Body,
) -> ApiResult<SavedSkinRecord> {
    let parameters = query(parameters)?;
    let variant = variant(parameters.variant.as_deref())?;
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
        .await?;
    Ok(Json(record))
}

async fn save_profile(
    State(service): State<SkinState>,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
    body: Body,
) -> ApiResult<SavedSkinRecord> {
    let parameters = query(parameters)?;
    let request: FromProfile = json_body(body).await?;
    let record = service
        .save_profile(
            request.name,
            variant(request.variant.as_deref())?,
            request.mark_current.unwrap_or(false),
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
        )
        .await?;
    Ok(Json(record))
}

async fn save_username(State(service): State<SkinState>, body: Body) -> ApiResult<SavedSkinRecord> {
    let request: FromUsername = json_body(body).await?;
    let record = service
        .save_username(
            &request.username,
            request.name,
            variant(request.variant.as_deref())?,
        )
        .await?;
    Ok(Json(record))
}

async fn remove(State(service): State<SkinState>, Path(key): Path<String>) -> ApiResult<Status> {
    match service.delete_saved(&key).await? {
        SavedSkinDeleteResult::Deleted(_) => Ok(Json(Status::new("deleted"))),
        SavedSkinDeleteResult::Missing => Err(ProfileMediaError::SavedSkinMissing.into()),
        SavedSkinDeleteResult::Applied => Err(ApiError::new(
            StatusCode::CONFLICT,
            "Reset or apply another skin before deleting the applied saved skin.",
        )),
    }
}

async fn update(
    State(service): State<SkinState>,
    Path(key): Path<String>,
    body: Body,
) -> ApiResult<SavedSkinRecord> {
    let request: UpdateSavedSkinRequest = json_body(body).await?;
    let record = service
        .update_saved(&key, request)
        .await?
        .ok_or(ProfileMediaError::SavedSkinMissing)?;
    Ok(Json(record))
}

async fn replace(
    State(service): State<SkinState>,
    Path(key): Path<String>,
    parameters: Result<Query<ReplaceQuery>, QueryRejection>,
    body: Body,
) -> ApiResult<SavedSkinRecord> {
    let parameters = query(parameters)?;
    let cape_id = match (parameters.clear_cape, parameters.cape_id) {
        (Some(true), Some(_)) => return Err(invalid_request()),
        (Some(true), None) => CapeUpdate::Clear,
        (_, cape_id) => cape_id.map(CapeUpdate::Set).unwrap_or_default(),
    };
    let variant = variant(parameters.variant.as_deref())?;
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
        .await?
        .ok_or(ProfileMediaError::SavedSkinMissing)?;
    Ok(Json(record))
}

async fn saved_file(State(service): State<SkinState>, Path(key): Path<String>) -> Media {
    let png = service
        .library
        .read_png(&key)?
        .ok_or(ProfileMediaError::SavedSkinMissing)?;
    Ok(image(png, true))
}

async fn apply(
    State(service): State<SkinState>,
    Path(key): Path<String>,
    parameters: Result<Query<ApplyQuery>, QueryRejection>,
) -> ApiResult<SkinApplyResponse> {
    let parameters = query(parameters)?;
    let accepted = service.queue(
        &key,
        &parameters.expected_account_id,
        parameters.expected_selection_revision,
    )?;
    if parameters.defer.unwrap_or(false) {
        return Ok(Json(SkinApplyResponse::queued(key, accepted)));
    }
    service
        .flush_generation(&accepted.account_id, accepted.generation)
        .await?;
    Ok(Json(SkinApplyResponse::applied(key)))
}

async fn flush(
    State(service): State<SkinState>,
    parameters: Result<Query<PendingCommandQuery>, QueryRejection>,
) -> ApiResult<SkinFlushResponse> {
    let parameters = query(parameters)?;
    let applied = service
        .flush_selected(
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
            parameters.expected_generation,
        )
        .await?;
    Ok(Json(SkinFlushResponse::new(applied)))
}

async fn cancel(
    State(service): State<SkinState>,
    parameters: Result<Query<PendingCommandQuery>, QueryRejection>,
) -> ApiResult<SkinCancelResponse> {
    let parameters = query(parameters)?;
    let cleared = service.cancel_selected(
        &parameters.expected_account_id,
        parameters.expected_selection_revision,
        parameters.expected_generation,
    )?;
    Ok(Json(SkinCancelResponse::new(cleared)))
}

async fn reset_skin(
    State(service): State<SkinState>,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
) -> ApiResult<ProfileResetResponse> {
    reset(service, parameters, true).await
}

async fn reset_cape(
    State(service): State<SkinState>,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
) -> ApiResult<ProfileResetResponse> {
    reset(service, parameters, false).await
}

async fn reset(
    service: SkinState,
    parameters: Result<Query<SelectionQuery>, QueryRejection>,
    skin: bool,
) -> ApiResult<ProfileResetResponse> {
    let parameters = query(parameters)?;
    service
        .reset(
            skin,
            &parameters.expected_account_id,
            parameters.expected_selection_revision,
        )
        .await?;
    Ok(Json(ProfileResetResponse::new(skin)))
}

#[cfg(test)]
mod tests {
    use crate::fixture::{Api, Profile};
    use axial_app::{
        accounts::{
            microsoft::MinecraftProfile, model::MicrosoftIdentity, selection::CapturedAccount,
        },
        skins::lookup::ProfileMediaError,
    };
    use reqwest::{Method, StatusCode};
    use serde_json::{Value, json};

    struct Fixture {
        profile: Profile,
        key: String,
    }

    impl Fixture {
        async fn new() -> Self {
            let profile = Profile::start().await;
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
            let (status, saved) = Api::json(
                profile
                    .api
                    .builder(Method::POST, "/api/v1/skins?name=Fixture%20skin")
                    .header("content-type", "image/png")
                    .body(png),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{saved}");
            Self {
                key: saved["texture_key"].as_str().unwrap().into(),
                profile,
            }
        }

        fn add_account(&self, profile_id: &str, name: &str) -> CapturedAccount {
            let accounts = &self.profile.services.accounts;
            accounts
                .commit_microsoft(
                    accounts.selection_revision().unwrap(),
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

        fn select(&self, capture: &CapturedAccount) -> CapturedAccount {
            let accounts = &self.profile.services.accounts;
            accounts.select(capture.account_id()).unwrap();
            accounts.capture_selected().unwrap()
        }

        /// The selected account's queued skin, as listed with its wardrobe.
        async fn pending(&self) -> (StatusCode, Value) {
            let (status, body) = self.request(Method::GET, "/api/v1/skins").await;
            let mut pending = body["pending"].clone();
            // The countdown changes between reads; the queued choice does not.
            if let Some(pending) = pending.as_object_mut() {
                pending.remove("due_in_ms");
            }
            (status, pending)
        }

        async fn request(&self, method: Method, path: &str) -> (StatusCode, Value) {
            self.profile.api.response(method, path, None).await
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
            assert_eq!(body["status"], "queued");
            assert_eq!(body["view_model"], json!({"summary":"Skin queued."}));
            assert_eq!(body["pending"]["account_id"], capture.account_id());
            assert_eq!(body["pending"]["texture_key"], self.key);
            body["pending"]["generation"].as_u64().unwrap()
        }

        async fn finish(self) {
            let services = &self.profile.services;
            for account in services.accounts.snapshot().unwrap().accounts {
                services.skins.account_removed(account.account_id.as_str());
            }
            self.profile.close().await;
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
        let fixture = Fixture::new().await;
        let first = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let second = fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
        let first = fixture.select(&first);
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
        let second = fixture.select(&second);
        fixture.queue(&second).await;
        let before = fixture.pending().await;
        for (method, path) in &delayed {
            let (status, body) = fixture.request(method.clone(), path).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .contains("reload the account")
            );
            assert_eq!(fixture.pending().await, before);
        }
        fixture
            .profile
            .services
            .accounts
            .select(first.account_id())
            .unwrap();
        let before = fixture.pending().await;
        assert_eq!(before.1["generation"], first_generation);
        for (method, path) in &delayed {
            assert_eq!(
                fixture.request(method.clone(), path).await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(fixture.pending().await, before);
        }
        fixture.finish().await;
    }

    async fn assert_stale_reset_preserves_pending(route: &str, reselected: bool) {
        let fixture = Fixture::new().await;
        let first = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let second = fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
        let confirmed = fixture.select(&first);
        let delayed = format!("{route}?{}", selection_query(&confirmed));
        fixture
            .profile
            .services
            .accounts
            .select(second.account_id())
            .unwrap();
        if reselected {
            fixture
                .profile
                .services
                .accounts
                .select(first.account_id())
                .unwrap();
        }
        let current = fixture
            .profile
            .services
            .accounts
            .capture_selected()
            .unwrap();
        fixture.queue(&current).await;
        let before = fixture.pending().await;
        assert_eq!(before.1["phase"], "queued");
        let (status, body) = fixture.request(Method::POST, &delayed).await;
        let after = fixture.pending().await;
        fixture.finish().await;
        assert_eq!(
            after, before,
            "stale reset must not cancel the current choice"
        );
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn stale_profile_resets_cannot_cancel_another_or_a_reselected_accounts_pending_skin() {
        for reselected in [false, true] {
            for route in ["/api/v1/skin/profile/reset", "/api/v1/skin/cape/reset"] {
                assert_stale_reset_preserves_pending(route, reselected).await;
            }
        }
    }

    #[tokio::test]
    async fn profile_reset_requires_a_complete_valid_selection_fence_before_cancelling() {
        let fixture = Fixture::new().await;
        let capture = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        fixture.queue(&capture).await;
        let before = fixture.pending().await;
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
                assert_eq!(fixture.pending().await, before);
            }
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn current_profile_reset_only_cancels_its_captured_pending_skin() {
        for route in ["/api/v1/skin/profile/reset", "/api/v1/skin/cape/reset"] {
            let fixture = Fixture::new().await;
            let first = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
            fixture.queue(&first).await;
            let second = fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
            fixture.queue(&second).await;
            let other = fixture.pending().await;
            let current = fixture.select(&first);
            let path = format!("{route}?{}", selection_query(&current));
            let (status, _) = fixture.request(Method::POST, &path).await;
            let after = fixture.pending().await;
            fixture
                .profile
                .services
                .accounts
                .select(second.account_id())
                .unwrap();
            let other_after = fixture.pending().await;
            fixture.finish().await;
            // Admission cancels only its target before the absent fixture credentials refuse I/O.
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(after.1["phase"], "idle");
            assert_eq!(other_after, other);
        }
    }

    #[tokio::test]
    async fn profile_save_rejects_stale_selection_before_profile_access() {
        let fixture = Fixture::new().await;
        let confirmed = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        fixture.add_account("22345678123442348234123456789abc", "PlayerTwo");
        let path = format!("/api/v1/skins/from-profile?{}", selection_query(&confirmed));
        for reselected in [false, true] {
            if reselected {
                fixture
                    .profile
                    .services
                    .accounts
                    .select(confirmed.account_id())
                    .unwrap();
            }
            let before = fixture.request(Method::GET, "/api/v1/skins").await;
            let (status, body) = fixture
                .profile
                .api
                .response(
                    Method::POST,
                    &path,
                    Some(json!({"variant":"classic","mark_current":true})),
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
        let fixture = Fixture::new().await;
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
                .profile
                .api
                .response(
                    Method::POST,
                    &format!("/api/v1/skins/from-profile{suffix}"),
                    Some(json!({"variant":"classic","mark_current":true})),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(fixture.request(Method::GET, "/api/v1/skins").await, before);
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn stale_pending_generation_cannot_cancel_or_flush_a_newer_choice() {
        let fixture = Fixture::new().await;
        let capture = fixture.add_account("12345678123442348234123456789abc", "PlayerOne");
        let old_generation = fixture.queue(&capture).await;
        let current_generation = fixture.queue(&capture).await;
        assert!(current_generation > old_generation);
        let before = fixture.pending().await;
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
            assert_eq!(fixture.pending().await, before);
        }
        let (status, body) = fixture
            .request(
                Method::DELETE,
                &pending_path("pending", &capture, current_generation),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cleared"], true);
        let (_, after) = fixture.pending().await;
        assert_eq!(after["phase"], "idle");
        assert!(after["texture_key"].is_null());
        fixture.finish().await;
    }

    #[tokio::test]
    async fn account_mutations_require_explicit_command_preconditions() {
        let fixture = Fixture::new().await;
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
        assert!(fixture.pending().await.1.is_null());
        fixture.finish().await;
    }
}
