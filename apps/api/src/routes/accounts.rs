use axial_app::accounts::{
    model::{
        AccountError, AccountPreconditions, AccountRecord, AccountSnapshot, offline_uuid,
        validate_username,
    },
    session::{AuthError, AuthService},
    view::AccountListResponse,
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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// Mount inside the authenticated local API boundary.
pub fn router(auth: Arc<AuthService>) -> Router {
    Router::new()
        .route("/api/v1/accounts", get(list))
        .route("/api/v1/accounts/offline", post(create))
        .route(
            "/api/v1/accounts/{account_id}",
            patch(rename).delete(remove),
        )
        .route("/api/v1/accounts/{account_id}/select", post(select))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(auth)
}

type ApiError = (StatusCode, Json<Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineAccountRequest {
    pub username: String,
    pub expected_selection_revision: Option<u64>,
    pub expected_account_revision: Option<u64>,
}

impl OfflineAccountRequest {
    fn expected(&self) -> AccountPreconditions {
        AccountPreconditions {
            expected_selection_revision: self.expected_selection_revision,
            expected_account_revision: self.expected_account_revision,
        }
    }
}

#[derive(Serialize)]
pub struct AccountCommandResponse {
    pub status: &'static str,
    pub selection_revision: u64,
    pub account: AccountRecord,
    pub view_model: CommandViewModel,
}

#[derive(Serialize)]
pub struct CommandViewModel {
    pub summary: &'static str,
}

async fn list(State(auth): State<Arc<AuthService>>) -> Result<Json<AccountListResponse>, ApiError> {
    auth.account_list().await.map(Json).map_err(auth_error)
}

async fn create(
    State(auth): State<Arc<AuthService>>,
    request: Result<Json<OfflineAccountRequest>, JsonRejection>,
) -> Result<Json<AccountCommandResponse>, ApiError> {
    let Json(request) = request.map_err(|_| invalid_request())?;
    let name = validate_username(&request.username).map_err(account_error)?;
    let expected = request.expected();
    let id = format!("offline-{}", offline_uuid(&name));
    let snapshot = mutate(&auth, move |directory| {
        directory.create_offline_with_preconditions(&name, expected)
    })
    .await?;
    command(
        snapshot,
        &id,
        "account_created",
        "Offline identity created.",
    )
}

async fn rename(
    State(auth): State<Arc<AuthService>>,
    Path(id): Path<String>,
    request: Result<Json<OfflineAccountRequest>, JsonRejection>,
) -> Result<Json<AccountCommandResponse>, ApiError> {
    let Json(request) = request.map_err(|_| invalid_request())?;
    let name = validate_username(&request.username).map_err(account_error)?;
    let expected = request.expected();
    let next_id = format!("offline-{}", offline_uuid(&name));
    let snapshot = mutate(&auth, move |directory| {
        directory.rename_offline_with_preconditions(&id, &name, expected)
    })
    .await?;
    command(
        snapshot,
        &next_id,
        "account_updated",
        "Offline identity updated.",
    )
}

async fn select(
    State(auth): State<Arc<AuthService>>,
    Path(id): Path<String>,
    request: Result<Json<AccountPreconditions>, JsonRejection>,
) -> Result<Json<AccountCommandResponse>, ApiError> {
    let Json(expected) = request.map_err(|_| invalid_request())?;
    let snapshot = auth
        .select_account(id.clone(), expected)
        .await
        .map_err(auth_error)?;
    command(snapshot, &id, "account_selected", "Account selected.")
}

async fn remove(
    State(auth): State<Arc<AuthService>>,
    Path(id): Path<String>,
    request: Result<Query<AccountPreconditions>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(expected) = request.map_err(|_| invalid_request())?;
    let snapshot = auth
        .remove_account(id.clone(), expected)
        .await
        .map_err(auth_error)?;
    Ok(Json(json!({"status":"account_removed", "account_id":id,
        "selection_revision":snapshot.selection_revision, "view_model":{"summary":"Account removed."}})))
}

async fn mutate(
    auth: &Arc<AuthService>,
    apply: impl FnOnce(
        &axial_app::accounts::directory::AccountDirectory,
    ) -> Result<AccountSnapshot, AccountError>
    + Send
    + 'static,
) -> Result<AccountSnapshot, ApiError> {
    let directory = auth.directory().clone();
    auth.task_owner()
        .try_spawn((), move |_| async move {
            tokio::task::spawn_blocking(move || apply(&directory))
                .await
                .map_err(|_| AuthError::Unavailable)?
                .map_err(AuthError::from)
        })
        .map_err(|_| auth_error(AuthError::Unavailable))?
        .join()
        .await
        .map_err(|_| auth_error(AuthError::Unavailable))?
        .map_err(auth_error)
}

fn command(
    snapshot: AccountSnapshot,
    id: &str,
    status: &'static str,
    summary: &'static str,
) -> Result<Json<AccountCommandResponse>, ApiError> {
    let account = snapshot
        .accounts
        .into_iter()
        .find(|account| account.account_id.as_str() == id)
        .ok_or_else(|| account_error(AccountError::NotFound))?;
    Ok(Json(AccountCommandResponse {
        status,
        selection_revision: snapshot.selection_revision,
        account,
        view_model: CommandViewModel { summary },
    }))
}

pub(super) fn invalid_request() -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"Invalid account request.","status":"account_input_invalid"})),
    )
}

pub(super) fn account_error(error: AccountError) -> ApiError {
    let (status, code) = match &error {
        AccountError::InvalidInput(_) | AccountError::NotOffline | AccountError::NotMicrosoft => {
            (StatusCode::BAD_REQUEST, "account_input_invalid")
        }
        AccountError::NotFound => (StatusCode::NOT_FOUND, "account_not_found"),
        AccountError::StaleCapture | AccountError::AlreadyExists => {
            (StatusCode::CONFLICT, "account_changed")
        }
        AccountError::NoSelection => (StatusCode::PRECONDITION_FAILED, "account_required"),
        AccountError::InvalidStoredData | AccountError::Storage => {
            (StatusCode::SERVICE_UNAVAILABLE, "account_unavailable")
        }
    };
    (
        status,
        Json(json!({"error":error.to_string(),"status":code})),
    )
}

pub(super) fn auth_error(error: AuthError) -> ApiError {
    if let AuthError::Account(error) = error {
        return account_error(error);
    }
    let (status, code) = match &error {
        AuthError::Account(_) => unreachable!("account errors returned above"),
        AuthError::SignInRequired | AuthError::LoginExpired => {
            (StatusCode::PRECONDITION_FAILED, "sign_in_required")
        }
        AuthError::OwnershipMissing => (StatusCode::CONFLICT, "minecraft_ownership_missing"),
        AuthError::Credentials(axial_app::accounts::credential_store::CredentialError::Stale) => {
            (StatusCode::CONFLICT, "account_changed")
        }
        AuthError::Credentials(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_store_unavailable",
        ),
        AuthError::Provider(_) => (StatusCode::BAD_GATEWAY, "minecraft_auth_chain_failed"),
        AuthError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "account_unavailable"),
    };
    (
        status,
        Json(json!({"error":error.to_string(),"status":code})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        accounts::{credential_store::CredentialStore, directory::AccountDirectory},
        storage::MetadataStore,
        tasks::TaskOwner,
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    async fn request(
        router: &Router,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn offline_http_journey_reports_renamed_identity_and_rejects_stale_selection() {
        let tasks = TaskOwner::new(32).unwrap();
        let directory =
            Arc::new(AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap());
        // Opening the store performs no OS I/O; this offline-only journey must
        // not consult secure entries or provider endpoints.
        let credentials = Arc::new(CredentialStore::with_task_owner(
            uuid::Uuid::new_v4(),
            tasks.clone(),
        ));
        let auth = Arc::new(AuthService::new(directory, credentials, tasks));
        let app = router(auth.clone()).merge(super::super::auth::router(auth, false));
        let (status, created) = request(
            &app,
            "POST",
            "/api/v1/accounts/offline",
            json!({"username":"Steve","expected_selection_revision":0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let old_id = created["account"]["account_id"].as_str().unwrap();
        let (status, renamed) = request(&app, "PATCH", &format!("/api/v1/accounts/{old_id}"),
            json!({"username":"Notch","expected_selection_revision":1,"expected_account_revision":1})).await;
        assert_eq!(status, StatusCode::OK);
        let new_id = renamed["account"]["account_id"].as_str().unwrap();
        assert_ne!(old_id, new_id);
        assert_eq!(
            renamed["account"]["offline_uuid"],
            "b50ad385829d3141a2167e7d7539ba7f"
        );
        let (status, _) = request(
            &app,
            "POST",
            &format!("/api/v1/accounts/{new_id}/select"),
            json!({"expected_selection_revision":1}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (_, accounts) = request(&app, "GET", "/api/v1/accounts", Value::Null).await;
        let (_, status) = request(&app, "GET", "/api/v1/auth/status", Value::Null).await;
        assert_eq!(accounts["selection_revision"], status["selection_revision"]);
        assert_eq!(accounts["accounts"][0]["active"], true);
        assert_eq!(status["username"], "Notch");
        assert_eq!(status["online_mode_ready"], false);
        let (status, _) = request(&app, "DELETE", &format!("/api/v1/accounts/{new_id}?expected_selection_revision=2&expected_account_revision=2"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let (_, accounts) = request(&app, "GET", "/api/v1/accounts", Value::Null).await;
        assert_eq!(accounts["accounts"].as_array().unwrap().len(), 0);
    }
}
