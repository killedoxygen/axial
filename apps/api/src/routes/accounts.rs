use super::error::{ApiError, ApiResult, Extracted, account_failure};
use axial_app::accounts::{
    model::AccountPreconditions,
    session::AuthService,
    view::{AccountCommandResponse, AccountListResponse, AccountRemovedResponse},
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, patch, post},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
struct Accounts {
    auth: Arc<AuthService>,
    sign_in_available: bool,
}

/// Mount inside the authenticated local API boundary. The native composition sets
/// `sign_in_available` only after its sign-in window commands are registered.
pub fn router(auth: Arc<AuthService>, sign_in_available: bool) -> Router {
    Router::new()
        .route("/api/v1/accounts", get(list))
        .route("/api/v1/accounts/offline", post(create))
        .route(
            "/api/v1/accounts/{account_id}",
            patch(rename).delete(remove),
        )
        .route("/api/v1/accounts/{account_id}/select", post(select))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(Accounts {
            auth,
            sign_in_available,
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineAccountRequest {
    username: String,
    expected_selection_revision: Option<u64>,
    expected_account_revision: Option<u64>,
}

impl OfflineAccountRequest {
    fn expected(&self) -> AccountPreconditions {
        AccountPreconditions {
            expected_selection_revision: self.expected_selection_revision,
            expected_account_revision: self.expected_account_revision,
        }
    }
}

fn invalid_request() -> ApiError {
    account_failure(
        StatusCode::BAD_REQUEST,
        "Invalid account request.".into(),
        "account_input_invalid",
    )
}

async fn list(State(state): State<Accounts>) -> ApiResult<AccountListResponse> {
    Ok(Json(
        state.auth.account_list(state.sign_in_available).await?,
    ))
}

async fn create(
    State(Accounts { auth, .. }): State<Accounts>,
    request: Result<Json<OfflineAccountRequest>, JsonRejection>,
) -> ApiResult<AccountCommandResponse> {
    let request = request.or_refuse(invalid_request)?;
    let expected = request.expected();
    Ok(Json(
        auth.create_offline(&request.username, expected).await?,
    ))
}

async fn rename(
    State(Accounts { auth, .. }): State<Accounts>,
    Path(id): Path<String>,
    request: Result<Json<OfflineAccountRequest>, JsonRejection>,
) -> ApiResult<AccountCommandResponse> {
    let request = request.or_refuse(invalid_request)?;
    let expected = request.expected();
    Ok(Json(
        auth.rename_offline(id, &request.username, expected).await?,
    ))
}

async fn select(
    State(Accounts { auth, .. }): State<Accounts>,
    Path(id): Path<String>,
    request: Result<Json<AccountPreconditions>, JsonRejection>,
) -> ApiResult<AccountCommandResponse> {
    let expected = request.or_refuse(invalid_request)?;
    let snapshot = auth.select_account(id.clone(), expected).await?;
    Ok(Json(AccountCommandResponse::new(
        snapshot,
        &id,
        "account_selected",
        "Account selected.",
    )?))
}

async fn remove(
    State(Accounts { auth, .. }): State<Accounts>,
    Path(id): Path<String>,
    request: Result<Query<AccountPreconditions>, QueryRejection>,
) -> ApiResult<AccountRemovedResponse> {
    let expected = request.or_refuse(invalid_request)?;
    let snapshot = auth.remove_account(id.clone(), expected).await?;
    Ok(Json(AccountRemovedResponse::new(id, &snapshot)))
}

#[cfg(test)]
mod tests {
    use crate::fixture::Profile;
    use reqwest::{Method, StatusCode};
    use serde_json::json;

    #[tokio::test]
    async fn offline_journey_reports_renamed_identity_and_rejects_stale_selection() {
        let profile = Profile::start().await;
        let api = &profile.api;
        let created = api
            .post(
                "/api/v1/accounts/offline",
                json!({"username":"Steve","expected_selection_revision":0}),
            )
            .await;
        assert_eq!(created["status"], "account_created");
        assert_eq!(
            created["view_model"],
            json!({"summary":"Offline identity created."})
        );
        let old_id = created["account"]["account_id"].as_str().unwrap();
        let renamed = api
            .request(
                Method::PATCH,
                &format!("/api/v1/accounts/{old_id}"),
                Some(json!({"username":"Notch","expected_selection_revision":1,"expected_account_revision":1})),
            )
            .await;
        let new_id = renamed["account"]["account_id"].as_str().unwrap();
        assert_ne!(old_id, new_id);
        assert_eq!(
            renamed["account"]["offline_uuid"],
            "b50ad385829d3141a2167e7d7539ba7f"
        );
        let (status, stale) = api
            .response(
                Method::POST,
                &format!("/api/v1/accounts/{new_id}/select"),
                Some(json!({"expected_selection_revision":1})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(stale["status"], "account_changed");
        let accounts = api.get("/api/v1/accounts").await;
        assert_eq!(accounts["selection_revision"], 2);
        assert_eq!(accounts["sign_in_available"], false);
        assert_eq!(accounts["accounts"][0]["active"], true);
        assert_eq!(accounts["accounts"][0]["display_name"], "Notch");
        let removed = api
            .request(
                Method::DELETE,
                &format!("/api/v1/accounts/{new_id}?expected_selection_revision=2&expected_account_revision=2"),
                None,
            )
            .await;
        assert_eq!(
            removed,
            json!({"account_id":new_id,"selection_revision":3,"status":"account_removed",
                "view_model":{"summary":"Account removed."}})
        );
        assert_eq!(api.get("/api/v1/accounts").await["accounts"], json!([]));
        assert_eq!(
            api.response(
                Method::POST,
                "/api/v1/accounts/offline",
                Some(json!({"name":"x"}))
            )
            .await,
            (
                StatusCode::BAD_REQUEST,
                json!({"error":"Invalid account request.","status":"account_input_invalid"})
            )
        );
        profile.close().await;
    }
}
