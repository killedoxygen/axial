use super::accounts::auth_error;
use axial_app::accounts::{session::AuthService, view::AuthStatusResponse};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct AuthRoutes {
    auth: Arc<AuthService>,
    login_available: bool,
}

/// The native composition may set login_available only after its OAuth window
/// commands are registered. Browser composition always supplies false.
pub fn router(auth: Arc<AuthService>, login_available: bool) -> Router {
    Router::new()
        .route("/api/v1/auth/status", get(status))
        .route("/api/v1/auth/refresh", post(refresh))
        .route("/api/v1/auth/profile/sync", post(sync))
        .route("/api/v1/auth/logout", post(logout))
        .with_state(AuthRoutes {
            auth,
            login_available,
        })
}

type ApiError = (StatusCode, Json<Value>);

async fn status(State(state): State<AuthRoutes>) -> Result<Json<AuthStatusResponse>, ApiError> {
    state
        .auth
        .status(state.login_available)
        .await
        .map(Json)
        .map_err(auth_error)
}

async fn refresh(State(state): State<AuthRoutes>) -> Result<Json<Value>, ApiError> {
    let capture = state.auth.refresh_selected().await.map_err(auth_error)?;
    Ok(Json(
        json!({"status":"refreshed", "account_id":capture.account_id(),
        "selection_revision":capture.selection_revision(), "minecraft_profile_ready":true,
        "minecraft_ownership_verified":capture.owns_minecraft_java(), "minecraft_profile":capture.profile(),
        "view_model":{"summary":"Microsoft sign-in refreshed."}}),
    ))
}

async fn sync(State(state): State<AuthRoutes>) -> Result<Json<Value>, ApiError> {
    let capture = state
        .auth
        .sync_selected_profile()
        .await
        .map_err(auth_error)?;
    Ok(Json(
        json!({"status":"profile_synced", "account_id":capture.account_id(),
        "selection_revision":capture.selection_revision(), "minecraft_profile_ready":true,
        "minecraft_ownership_verified":capture.owns_minecraft_java(), "minecraft_profile":capture.profile(),
        "view_model":{"summary":"Minecraft profile synced."}}),
    ))
}

async fn logout(State(state): State<AuthRoutes>) -> Result<Json<Value>, ApiError> {
    let snapshot = state.auth.logout().await.map_err(auth_error)?;
    Ok(Json(
        json!({"status":"logged_out", "selection_revision":snapshot.selection_revision,
        "view_model":{"summary":"Signed out of Microsoft."}}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        accounts::{
            credential_store::CredentialStore, credentials::Credentials,
            directory::AccountDirectory, microsoft::MinecraftProfile, model::MicrosoftIdentity,
            session::AuthError,
        },
        storage::MetadataStore,
        tasks::TaskOwner,
    };
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, Request},
    };
    use std::{
        path::PathBuf,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    const PROFILE: &str = "12345678123442348234123456789abc";

    #[derive(Clone)]
    struct Provider {
        profile: Arc<Mutex<(StatusCode, Value)>>,
        entitlements: Arc<Mutex<(StatusCode, Value)>>,
        pause: Arc<Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>,
    }

    fn profile() -> Value {
        json!({"id":PROFILE,"name":"CurrentPlayer","skins":[],"capes":[]})
    }

    async fn provider_profile(
        State(state): State<Provider>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert!(headers.get("authorization").is_some());
        let pause = state.pause.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.send(()).unwrap();
            resume.await.unwrap();
        }
        let (status, body) = state.profile.lock().unwrap().clone();
        (status, Json(body))
    }

    async fn provider_entitlements(
        State(state): State<Provider>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert!(headers.get("authorization").is_some());
        let (status, body) = state.entitlements.lock().unwrap().clone();
        (status, Json(body))
    }

    struct Fixture {
        _root: tempfile::TempDir,
        path: PathBuf,
        metadata: Arc<MetadataStore>,
        directory: Arc<AccountDirectory>,
        credentials: Arc<CredentialStore>,
        auth: Arc<AuthService>,
        app: Router,
        bundle: Credentials,
        provider: Provider,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn fixture() -> Fixture {
        let provider = Provider {
            profile: Arc::new(Mutex::new((StatusCode::OK, profile()))),
            entitlements: Arc::new(Mutex::new((
                StatusCode::OK,
                json!({"items":[{"name":"game_minecraft"}]}),
            ))),
            pause: Arc::default(),
        };
        let provider_app = Router::new()
            .route("/profile", get(provider_profile))
            .route("/entitlements", get(provider_entitlements))
            .with_state(provider.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tokio::spawn(async move { axum::serve(listener, provider_app).await.unwrap() });
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("accounts.sqlite");
        let metadata = Arc::new(MetadataStore::open(&path).unwrap());
        let directory = Arc::new(AccountDirectory::new(metadata.clone()).unwrap());
        let credentials = Arc::new(CredentialStore::isolated_for_tests());
        let id = uuid::Uuid::parse_str(PROFILE)
            .unwrap()
            .hyphenated()
            .to_string();
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let bundle = Credentials::new(
            "synthetic-microsoft".into(),
            None,
            expiry,
            "synthetic-minecraft".into(),
            expiry,
        )
        .unwrap();
        let fence = credentials.begin_change(&id, 0).await.unwrap();
        let receipt = credentials.save(&fence, bundle.clone()).await.unwrap();
        directory
            .commit_microsoft(
                0,
                MicrosoftIdentity {
                    login_id: uuid::Uuid::new_v4().to_string(),
                    profile_id: PROFILE.into(),
                    display_name: "OldPlayer".into(),
                    credential_revision: receipt.revision(),
                    owns_minecraft_java: true,
                    profile: MinecraftProfile {
                        id: PROFILE.into(),
                        name: "OldPlayer".into(),
                        skins: vec![],
                        capes: vec![],
                    },
                },
            )
            .unwrap();
        let auth = Arc::new(
            AuthService::new(
                directory.clone(),
                credentials.clone(),
                TaskOwner::new(16).unwrap(),
            )
            .with_profile_endpoints_for_tests(&base),
        );
        let app = router(auth.clone(), false).merge(super::super::accounts::router(auth.clone()));
        Fixture {
            _root: root,
            path,
            metadata,
            directory,
            credentials,
            auth,
            app,
            bundle,
            provider,
            server,
        }
    }

    async fn request(app: &Router, method: &str, path: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.matches("\"minecraft_ownership_verified\"").count() <= 1);
        assert!(!text.contains("owns_minecraft_java") && !text.contains("synthetic-"));
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn sync_keeps_credentials_and_reports_negative_ownership() {
        let f = fixture().await;
        let original = f.directory.capture_selected().unwrap();
        let credential_status = f.credentials.status(original.account_id()).await.unwrap();
        let (status, positive) = request(&f.app, "POST", "/api/v1/auth/profile/sync").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(positive["minecraft_ownership_verified"], true);
        assert_eq!(positive["minecraft_profile"]["name"], "CurrentPlayer");

        *f.provider.entitlements.lock().unwrap() = (StatusCode::OK, json!({"items":[]}));
        let before_negative = f.directory.capture_selected().unwrap();
        let (status, negative) = request(&f.app, "POST", "/api/v1/auth/profile/sync").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(negative["status"], "profile_synced");
        assert_eq!(negative["minecraft_ownership_verified"], false);
        assert_eq!(negative["minecraft_profile"]["name"], "CurrentPlayer");
        let capture = f.directory.capture_selected().unwrap();
        assert_eq!(capture.login_id(), original.login_id());
        assert_eq!(
            capture.credential_revision(),
            original.credential_revision()
        );
        assert!(capture.account_revision() > before_negative.account_revision());
        assert!(
            f.directory
                .validate_account_capture(&before_negative)
                .is_err()
        );
        assert_eq!(f.auth.credentials(&capture).await.unwrap(), f.bundle);
        assert_eq!(
            f.credentials.status(capture.account_id()).await.unwrap(),
            credential_status
        );
        assert!(matches!(
            f.auth.launch_credentials(&capture).await,
            Err(AuthError::OwnershipMissing)
        ));
        let (_, status) = request(&f.app, "GET", "/api/v1/auth/status").await;
        assert_eq!(status["minecraft_ownership_verified"], false);
        assert_eq!(status["minecraft_profile_ready"], true);
        assert_eq!(status["online_mode_ready"], false);
        assert_eq!(status["mode"], "offline");
        assert_eq!(status["provider"], "offline");
        assert_eq!(status["msa_provider"], "microsoft");
        assert_eq!(status["launch_auth_mode"], "online");
        assert_eq!(status["profile_sync_action"]["enabled"], true);
        assert_eq!(status["skin_action"]["enabled"], false);
        let (_, accounts) = request(&f.app, "GET", "/api/v1/accounts").await;
        assert_eq!(
            accounts["accounts"][0]["minecraft_ownership_verified"],
            false
        );

        let reopened = Arc::new(
            AccountDirectory::new(Arc::new(MetadataStore::open(&f.path).unwrap())).unwrap(),
        );
        assert_eq!(
            reopened.snapshot().unwrap(),
            f.directory.snapshot().unwrap()
        );
        let reopened_auth = AuthService::new(
            reopened.clone(),
            f.credentials.clone(),
            TaskOwner::new(8).unwrap(),
        );
        let reopened_capture = reopened.capture_selected().unwrap();
        assert!(!reopened_capture.owns_minecraft_java());
        assert_eq!(
            reopened_auth.credentials(&reopened_capture).await.unwrap(),
            f.bundle
        );
        assert!(matches!(
            reopened_auth.launch_credentials(&reopened_capture).await,
            Err(AuthError::OwnershipMissing)
        ));

        *f.provider.entitlements.lock().unwrap() = (
            StatusCode::OK,
            json!({"items":[{"name":"product_minecraft"}]}),
        );
        let (status, restored) = request(&f.app, "POST", "/api/v1/auth/profile/sync").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(restored["minecraft_ownership_verified"], true);
        let capture = f.directory.capture_selected().unwrap();
        assert_eq!(f.auth.launch_credentials(&capture).await.unwrap(), f.bundle);
        assert_eq!(
            f.credentials.status(capture.account_id()).await.unwrap(),
            credential_status
        );
    }

    #[tokio::test]
    async fn sync_provider_refusals_preserve_profile_ownership_and_credentials() {
        let f = fixture().await;
        let before = f.directory.snapshot().unwrap();
        let capture = f.directory.capture_selected().unwrap();
        let credential_status = f.credentials.status(capture.account_id()).await.unwrap();
        for (profile_reply, entitlement_reply, expected) in [
            (
                (StatusCode::BAD_GATEWAY, json!({})),
                (StatusCode::OK, json!({"items":[]})),
                StatusCode::BAD_GATEWAY,
            ),
            (
                (StatusCode::OK, json!({})),
                (StatusCode::OK, json!({"items":[]})),
                StatusCode::BAD_GATEWAY,
            ),
            (
                (StatusCode::OK, profile()),
                (StatusCode::BAD_GATEWAY, json!({})),
                StatusCode::BAD_GATEWAY,
            ),
            (
                (StatusCode::OK, profile()),
                (StatusCode::OK, json!({})),
                StatusCode::BAD_GATEWAY,
            ),
            (
                (
                    StatusCode::OK,
                    json!({"id":"22345678123442348234123456789abc","name":"OtherPlayer","skins":[],"capes":[]}),
                ),
                (StatusCode::OK, json!({"items":[]})),
                StatusCode::CONFLICT,
            ),
        ] {
            *f.provider.profile.lock().unwrap() = profile_reply;
            *f.provider.entitlements.lock().unwrap() = entitlement_reply;
            let (status, _) = request(&f.app, "POST", "/api/v1/auth/profile/sync").await;
            assert_eq!(status, expected);
            assert_eq!(f.directory.snapshot().unwrap(), before);
            assert_eq!(f.auth.credentials(&capture).await.unwrap(), f.bundle);
            assert_eq!(
                f.credentials.status(capture.account_id()).await.unwrap(),
                credential_status
            );
        }
    }

    #[tokio::test]
    async fn sync_stale_or_unpublished_completion_cannot_change_ownership() {
        let f = fixture().await;
        let original = f.directory.capture_selected().unwrap();
        *f.provider.entitlements.lock().unwrap() = (StatusCode::OK, json!({"items":[]}));
        let (entered, entered_wait) = oneshot::channel();
        let (resume, resume_wait) = oneshot::channel();
        *f.provider.pause.lock().unwrap() = Some((entered, resume_wait));
        let app = f.app.clone();
        let pending =
            tokio::spawn(async move { request(&app, "POST", "/api/v1/auth/profile/sync").await });
        entered_wait.await.unwrap();
        let selected = f.directory.create_offline_account("Steve").unwrap();
        resume.send(()).unwrap();
        assert_eq!(pending.await.unwrap().0, StatusCode::CONFLICT);
        assert_eq!(f.directory.snapshot().unwrap(), selected);
        assert_eq!(f.auth.credentials(&original).await.unwrap(), f.bundle);
        f.auth
            .select_account(original.account_id().into(), Default::default())
            .await
            .unwrap();
        let before = f.directory.snapshot().unwrap();
        f.metadata.transaction(|tx| -> Result<(), axial_app::storage::StorageError> {
            tx.execute_batch("CREATE TRIGGER ignore_account_sync BEFORE UPDATE ON account_directory BEGIN SELECT RAISE(IGNORE); END")?;
            Ok(())
        }).unwrap();
        let (status, _) = request(&f.app, "POST", "/api/v1/auth/profile/sync").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(f.directory.snapshot().unwrap(), before);
        let capture = f.directory.capture_selected().unwrap();
        assert!(capture.owns_minecraft_java());
        assert_eq!(f.auth.credentials(&capture).await.unwrap(), f.bundle);
    }
}
