use crate::{
    application::{
        self, AccountActionResponse, AccountListResponse, AccountPatchRequest,
        AccountRemoveResponse, OfflineAccountCreateRequest,
    },
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/accounts", get(handle_accounts))
        .route(
            "/api/v1/accounts/offline",
            post(handle_offline_account_create),
        )
        .route(
            "/api/v1/accounts/{account_id}",
            patch(handle_account_patch).delete(handle_account_remove),
        )
        .route(
            "/api/v1/accounts/{account_id}/select",
            post(handle_account_select),
        )
}

async fn handle_accounts(
    State(state): State<AppState>,
) -> Result<Json<AccountListResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::accounts(&state).await
}

async fn handle_offline_account_create(
    State(state): State<AppState>,
    Json(request): Json<OfflineAccountCreateRequest>,
) -> Result<Json<AccountActionResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::create_offline_account(&state, request).await
}

async fn handle_account_patch(
    Path(account_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<AccountPatchRequest>,
) -> Result<Json<AccountActionResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::patch_account(&state, &account_id, request).await
}

async fn handle_account_select(
    Path(account_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<AccountActionResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::select_account(&state, &account_id).await
}

async fn handle_account_remove(
    Path(account_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<AccountRemoveResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::remove_account(&state, &account_id).await
}
