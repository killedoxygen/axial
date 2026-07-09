use crate::application::skin as application_skin;
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::{delete, get, post, put},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/skin/profile", get(handle_skin_profile))
        .route(
            "/api/v1/skin/profile/reset",
            post(handle_skin_profile_reset),
        )
        .route("/api/v1/skin/profile/file", get(handle_skin_profile_file))
        .route("/api/v1/skin/cape/file", get(handle_skin_cape_file))
        .route("/api/v1/skin/cape/reset", post(handle_skin_cape_reset))
        .route("/api/v1/skin/head", get(handle_skin_head))
        .route("/api/v1/skin/lookup", get(handle_skin_lookup))
        .route("/api/v1/skin/lookup/file", get(handle_skin_lookup_file))
        .route("/api/v1/skin/lookup/head", get(handle_skin_lookup_head))
        .route("/api/v1/skin/lookup/cape", get(handle_skin_lookup_cape))
        .route("/api/v1/skins/normalize", post(handle_skin_normalize))
        .route(
            "/api/v1/skins",
            get(handle_saved_skins).post(handle_save_skin),
        )
        .route(
            "/api/v1/skins/from-profile",
            post(handle_save_skin_from_profile),
        )
        .route(
            "/api/v1/skins/from-username",
            post(handle_save_skin_from_username),
        )
        .route(
            "/api/v1/skins/pending",
            delete(handle_clear_pending_saved_skin_apply),
        )
        .route(
            "/api/v1/skins/{texture_key}",
            delete(handle_delete_skin).put(handle_update_saved_skin),
        )
        .route(
            "/api/v1/skins/{texture_key}/texture",
            put(handle_replace_saved_skin_texture),
        )
        .route(
            "/api/v1/skins/{texture_key}/file",
            get(handle_saved_skin_file),
        )
        .route(
            "/api/v1/skins/{texture_key}/apply",
            post(handle_apply_saved_skin),
        )
        .route("/api/v1/skins/flush", post(handle_flush_saved_skin_applies))
}

async fn handle_skin_profile(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_profile(&state, query).await
}

async fn handle_skin_profile_reset(State(state): State<AppState>) -> impl IntoResponse {
    application_skin::handle_skin_profile_reset(&state).await
}

async fn handle_skin_profile_file(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinProfileFileQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_profile_file(&state, query).await
}

async fn handle_skin_cape_file(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinCapeFileQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_cape_file(&state, query).await
}

async fn handle_skin_cape_reset(State(state): State<AppState>) -> impl IntoResponse {
    application_skin::handle_skin_cape_reset(&state).await
}

async fn handle_skin_head(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_head(&state, query).await
}

async fn handle_skin_lookup(
    Query(query): Query<application_skin::SkinLookupQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_lookup(query).await
}

async fn handle_skin_lookup_file(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinLookupQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_lookup_file(&state, query).await
}

async fn handle_skin_lookup_head(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinLookupQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_lookup_head(&state, query).await
}

async fn handle_skin_lookup_cape(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SkinLookupQuery>,
) -> impl IntoResponse {
    application_skin::handle_skin_lookup_cape(&state, query).await
}

async fn handle_skin_normalize(body: Body) -> impl IntoResponse {
    application_skin::handle_skin_normalize(body).await
}

async fn handle_saved_skins(State(state): State<AppState>) -> impl IntoResponse {
    application_skin::handle_saved_skins(&state).await
}

async fn handle_save_skin(
    State(state): State<AppState>,
    Query(query): Query<application_skin::SaveSkinQuery>,
    body: Body,
) -> impl IntoResponse {
    application_skin::handle_save_skin(&state, query, body).await
}

async fn handle_save_skin_from_profile(
    State(state): State<AppState>,
    body: Body,
) -> impl IntoResponse {
    application_skin::handle_save_skin_from_profile(&state, body).await
}

async fn handle_save_skin_from_username(
    State(state): State<AppState>,
    body: Body,
) -> impl IntoResponse {
    application_skin::handle_save_skin_from_username(&state, body).await
}

async fn handle_clear_pending_saved_skin_apply(State(state): State<AppState>) -> impl IntoResponse {
    application_skin::handle_clear_pending_saved_skin_apply(&state).await
}

async fn handle_delete_skin(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
) -> impl IntoResponse {
    application_skin::handle_delete_skin(&state, texture_key).await
}

async fn handle_update_saved_skin(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
    Json(payload): Json<application_skin::UpdateSavedSkinRequest>,
) -> impl IntoResponse {
    application_skin::handle_update_saved_skin(&state, texture_key, payload).await
}

async fn handle_replace_saved_skin_texture(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
    Query(query): Query<application_skin::ReplaceSavedSkinTextureQuery>,
    body: Body,
) -> impl IntoResponse {
    application_skin::handle_replace_saved_skin_texture(&state, texture_key, query, body).await
}

async fn handle_saved_skin_file(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
) -> impl IntoResponse {
    application_skin::handle_saved_skin_file(&state, texture_key).await
}

async fn handle_apply_saved_skin(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
    Query(query): Query<application_skin::ApplySavedSkinQuery>,
) -> impl IntoResponse {
    application_skin::handle_apply_saved_skin(&state, texture_key, query).await
}

async fn handle_flush_saved_skin_applies(State(state): State<AppState>) -> impl IntoResponse {
    application_skin::handle_flush_saved_skin_applies(&state).await
}
