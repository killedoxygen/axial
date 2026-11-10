//! Selected-account profile sync against a loopback Minecraft services fixture.

use super::{
    credential_store::CredentialStore,
    credentials::Credentials,
    directory::AccountDirectory,
    microsoft::MinecraftProfile,
    model::{AccountError, MicrosoftIdentity},
    session::{AuthError, AuthService},
    view::ProfileRefreshResponse,
};
use crate::{
    storage::{MetadataStore, StorageError},
    tasks::TaskOwner,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

const PROFILE: &str = "12345678123442348234123456789abc";

type Reply = Arc<Mutex<(StatusCode, Value)>>;

#[derive(Clone)]
struct Provider {
    profile: Reply,
    entitlements: Reply,
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
    auth: AuthService,
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
    let server = tokio::spawn(async move { axum::serve(listener, provider_app).await.unwrap() });
    let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
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
    let auth = AuthService::new(
        directory.clone(),
        credentials.clone(),
        TaskOwner::new(16).unwrap(),
    )
    .with_profile_endpoints_for_tests(&base);
    Fixture {
        _root: root,
        path,
        metadata,
        directory,
        credentials,
        auth,
        bundle,
        provider,
        server,
    }
}

impl Fixture {
    async fn sync(&self) -> Result<Value, AuthError> {
        let capture = self.auth.sync_selected_profile().await?;
        let response = serde_json::to_value(ProfileRefreshResponse::synced(&capture)).unwrap();
        let text = response.to_string();
        assert!(!text.contains("owns_minecraft_java") && !text.contains("synthetic-"));
        Ok(response)
    }
}

#[tokio::test]
async fn sync_keeps_credentials_and_reports_negative_ownership() {
    let f = fixture().await;
    let original = f.directory.capture_selected().unwrap();
    let credential_status = f.credentials.status(original.account_id()).await.unwrap();
    let positive = f.sync().await.unwrap();
    assert_eq!(positive["minecraft_ownership_verified"], true);
    assert_eq!(positive["minecraft_profile"]["name"], "CurrentPlayer");

    *f.provider.entitlements.lock().unwrap() = (StatusCode::OK, json!({"items":[]}));
    let before_negative = f.directory.capture_selected().unwrap();
    let negative = f.sync().await.unwrap();
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
    let accounts = serde_json::to_value(f.auth.account_list(false).await.unwrap()).unwrap();
    assert_eq!(accounts["launch_auth_mode"], "online");
    let account = &accounts["accounts"][0];
    assert_eq!(account["minecraft_ownership_verified"], false);
    assert_eq!(account["minecraft_profile_ready"], true);
    assert_eq!(account["online_mode_ready"], false);
    assert_eq!(account["profile_sync_action"]["enabled"], true);
    assert_eq!(account["skin_action"]["enabled"], false);

    let reopened =
        Arc::new(AccountDirectory::new(Arc::new(MetadataStore::open(&f.path).unwrap())).unwrap());
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
    assert_eq!(
        f.sync().await.unwrap()["minecraft_ownership_verified"],
        true
    );
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
    let other =
        json!({"id":"22345678123442348234123456789abc","name":"OtherPlayer","skins":[],"capes":[]});
    for (profile_reply, entitlement_reply, provider_failure) in [
        (
            (StatusCode::BAD_GATEWAY, json!({})),
            (StatusCode::OK, json!({"items":[]})),
            true,
        ),
        (
            (StatusCode::OK, json!({})),
            (StatusCode::OK, json!({"items":[]})),
            true,
        ),
        (
            (StatusCode::OK, profile()),
            (StatusCode::BAD_GATEWAY, json!({})),
            true,
        ),
        (
            (StatusCode::OK, profile()),
            (StatusCode::OK, json!({})),
            true,
        ),
        (
            (StatusCode::OK, other),
            (StatusCode::OK, json!({"items":[]})),
            false,
        ),
    ] {
        *f.provider.profile.lock().unwrap() = profile_reply;
        *f.provider.entitlements.lock().unwrap() = entitlement_reply;
        let error = f.sync().await.unwrap_err();
        assert_eq!(
            matches!(error, AuthError::Provider(_)),
            provider_failure,
            "{error}"
        );
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
    let f = Arc::new(fixture().await);
    let original = f.directory.capture_selected().unwrap();
    *f.provider.entitlements.lock().unwrap() = (StatusCode::OK, json!({"items":[]}));
    let (entered, entered_wait) = oneshot::channel();
    let (resume, resume_wait) = oneshot::channel();
    *f.provider.pause.lock().unwrap() = Some((entered, resume_wait));
    let pending = tokio::spawn({
        let f = f.clone();
        async move { f.sync().await.map(|_| ()) }
    });
    entered_wait.await.unwrap();
    let selected = f.directory.create_offline_account("Steve").unwrap();
    resume.send(()).unwrap();
    assert!(matches!(
        pending.await.unwrap(),
        Err(AuthError::Account(AccountError::StaleCapture))
    ));
    assert_eq!(f.directory.snapshot().unwrap(), selected);
    assert_eq!(f.auth.credentials(&original).await.unwrap(), f.bundle);
    f.auth
        .select_account(original.account_id().into(), Default::default())
        .await
        .unwrap();
    let before = f.directory.snapshot().unwrap();
    f.metadata.transaction(|tx| -> Result<(), StorageError> {
        tx.execute_batch("CREATE TRIGGER ignore_account_sync BEFORE UPDATE ON account_directory BEGIN SELECT RAISE(IGNORE); END")?;
        Ok(())
    }).unwrap();
    assert!(matches!(
        f.sync().await,
        Err(AuthError::Unavailable
            | AuthError::Account(AccountError::Storage | AccountError::InvalidStoredData))
    ));
    assert_eq!(f.directory.snapshot().unwrap(), before);
    let capture = f.directory.capture_selected().unwrap();
    assert!(capture.owns_minecraft_java());
    assert_eq!(f.auth.credentials(&capture).await.unwrap(), f.bundle);
}
