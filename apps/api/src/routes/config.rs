use std::sync::Arc;

use axial_app::{
    accounts::{
        directory::AccountDirectory,
        model::{AccountError, LaunchAuthMode},
    },
    settings::{
        ConfigLaunchAuthMode, ConfigPatch, ConfigView, InterfacePreferencesReceipt,
        InterfacePreferencesSnapshot, InterfacePreferencesUpdate, MAX_INTERFACE_PREFERENCES_BYTES,
        SettingsError, SettingsStore,
    },
    storage::rusqlite::Connection,
    tasks::TaskOwner,
    telemetry::{Telemetry, TelemetryErrorKind, TelemetryEvent},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) type ApiError = (StatusCode, Json<Value>);

#[derive(Clone)]
pub struct ConfigRouteState {
    settings: Arc<SettingsStore>,
    accounts: Arc<AccountDirectory>,
    telemetry: Arc<Telemetry>,
    tasks: TaskOwner,
}

impl ConfigRouteState {
    pub fn new(
        settings: Arc<SettingsStore>,
        accounts: Arc<AccountDirectory>,
        telemetry: Arc<Telemetry>,
        tasks: TaskOwner,
    ) -> Result<Self, SettingsError> {
        if !accounts.uses_metadata(settings.metadata())
            || settings.telemetry_identity_enabled() != telemetry.export_configured()
        {
            return Err(SettingsError::Unavailable);
        }
        Ok(Self {
            settings,
            accounts,
            telemetry,
            tasks,
        })
    }

    fn current(&self) -> Result<ConfigView, SettingsError> {
        self.settings
            .current_with_projection(|connection, config| self.project_accounts(connection, config))
    }

    fn project_accounts(
        &self,
        connection: &Connection,
        config: &mut ConfigView,
    ) -> Result<(), SettingsError> {
        // Construction verifies both owners use this exact metadata connection.
        debug_assert!(self.accounts.uses_metadata(self.settings.metadata()));
        config.account_selection_revision =
            AccountDirectory::selection_revision_in_transaction(connection)
                .map_err(account_error)?;
        match AccountDirectory::selection_in_transaction(connection).map_err(account_error)? {
            Some((username, mode)) => {
                config.username = username;
                config.launch_auth_mode = match mode {
                    LaunchAuthMode::Offline => ConfigLaunchAuthMode::Offline,
                    LaunchAuthMode::Online => ConfigLaunchAuthMode::Online,
                };
            }
            None => {
                config.launch_auth_mode = ConfigLaunchAuthMode::Offline;
                // The last persisted projection may belong to a removed online
                // account whose provider name is too short for offline input.
                if axial_app::settings::validate_username(&config.username).is_err() {
                    config.username = ConfigView::default().username;
                }
            }
        }
        Ok(())
    }
}

/// The composition mounts this complete route tree behind local API authority.
pub fn router(state: ConfigRouteState) -> Router {
    Router::new()
        .route("/api/v1/config", get(get_config).put(update_config))
        .route("/api/v1/onboarding/complete", post(complete_onboarding))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .merge(
            Router::new()
                .route(
                    "/api/v1/config/interface-preferences",
                    get(get_interface_preferences).put(update_interface_preferences),
                )
                .layer(DefaultBodyLimit::max(
                    MAX_INTERFACE_PREFERENCES_BYTES + 1024,
                )),
        )
        .with_state(state)
}

async fn get_interface_preferences(
    State(state): State<ConfigRouteState>,
) -> Result<Json<InterfacePreferencesSnapshot>, ApiError> {
    tokio::task::spawn_blocking(move || state.settings.interface_preferences())
        .await
        .map_err(|_| settings_error(SettingsError::Unavailable))?
        .map(Json)
        .map_err(settings_error)
}

async fn update_interface_preferences(
    State(state): State<ConfigRouteState>,
    request: Result<Json<InterfacePreferencesUpdate>, JsonRejection>,
) -> Result<Json<InterfacePreferencesReceipt>, ApiError> {
    let Json(update) = request.map_err(json_error)?;
    let tasks = state.tasks.clone();
    let handle = tasks
        .try_spawn((), move |cancellation| async move {
            tokio::task::spawn_blocking(move || {
                if cancellation.is_cancelled() {
                    return Err(SettingsError::Unavailable);
                }
                state.settings.update_interface_preferences(update)
            })
            .await
            .expect("interface preference persistence task panicked")
        })
        .map_err(|_| settings_error(SettingsError::Unavailable))?;
    handle
        .join()
        .await
        .map_err(|_| settings_error(SettingsError::Unavailable))?
        .map(Json)
        .map_err(settings_error)
}

async fn get_config(State(state): State<ConfigRouteState>) -> Result<Json<ConfigView>, ApiError> {
    tokio::task::spawn_blocking(move || state.current())
        .await
        .map_err(|_| settings_error(SettingsError::Unavailable))?
        .map(Json)
        .map_err(settings_error)
}

async fn update_config(
    State(state): State<ConfigRouteState>,
    request: Result<Json<ConfigPatch>, JsonRejection>,
) -> Result<Json<ConfigView>, ApiError> {
    let Json(patch) = request.map_err(json_error)?;
    commit_config(state, patch)
        .await
        .map(Json)
        .map_err(settings_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnboardingCompletion {
    expected_revision: u64,
}

#[derive(Serialize)]
struct OnboardingResponse {
    status: &'static str,
}

async fn complete_onboarding(
    State(state): State<ConfigRouteState>,
    request: Result<Json<OnboardingCompletion>, JsonRejection>,
) -> Result<Json<OnboardingResponse>, ApiError> {
    let Json(request) = request.map_err(json_error)?;
    commit_config(
        state,
        ConfigPatch {
            expected_revision: request.expected_revision,
            onboarding_done: Some(true),
            ..ConfigPatch::default()
        },
    )
    .await
    .map_err(settings_error)?;
    Ok(Json(OnboardingResponse { status: "ok" }))
}

async fn commit_config(
    state: ConfigRouteState,
    patch: ConfigPatch,
) -> Result<ConfigView, SettingsError> {
    let tasks = state.tasks.clone();
    let handle = tasks
        .try_spawn((), move |cancellation| async move {
            // The accepted task owns this lease through blocking persistence and
            // publication, even when HTTP disconnects or shutdown begins.
            let consent = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(SettingsError::Unavailable),
                consent = state.telemetry.consent_change_owned() => consent,
            };
            if cancellation.is_cancelled() {
                return Err(SettingsError::Unavailable);
            }
            tokio::task::spawn_blocking(move || {
                let rename = patch.username.is_some();
                let mode = patch.launch_auth_mode;
                let expected_selection = patch.expected_account_selection_revision;
                let suppress_failure = patch.telemetry_enabled == Some(false);
                let result =
                    state
                        .settings
                        .commit_with_transaction(patch, |transaction, config| {
                            if rename || mode.is_some() {
                                let expected = expected_selection.ok_or(SettingsError::Validation(
                                "Account selection revision is required for identity changes.",
                            ))?;
                                if AccountDirectory::selection_revision_in_transaction(transaction)
                                    .map_err(account_error)?
                                    != expected
                                {
                                    return Err(SettingsError::Conflict);
                                }
                            }
                            if let Some(mode) = mode {
                                AccountDirectory::select_launch_mode_in_transaction(
                                    transaction,
                                    match mode {
                                        ConfigLaunchAuthMode::Offline => LaunchAuthMode::Offline,
                                        ConfigLaunchAuthMode::Online => LaunchAuthMode::Online,
                                    },
                                )
                                .map_err(account_error)?;
                            }
                            if rename {
                                AccountDirectory::sync_active_offline_username_in_transaction(
                                    transaction,
                                    &config.username,
                                )
                                .map_err(account_error)?;
                            }
                            state.project_accounts(transaction, config)
                        });
                match result {
                    Ok(commit) => {
                        consent.publish(
                            commit.config.telemetry_enabled,
                            commit.telemetry_identity.as_deref(),
                        );
                        Ok(commit.config)
                    }
                    Err(error) => {
                        if !suppress_failure {
                            report_save_failure(&state.telemetry, &error);
                        }
                        Err(error)
                    }
                }
            })
            .await
            .expect("settings persistence task panicked")
        })
        .map_err(|_| SettingsError::Unavailable)?;
    handle
        .join()
        .await
        .map_err(|_| SettingsError::Unavailable)?
}

fn account_error(error: AccountError) -> SettingsError {
    match error {
        AccountError::InvalidInput(message) => SettingsError::Validation(message),
        AccountError::NoSelection | AccountError::NotFound => {
            SettingsError::Validation("Select an account for the requested launch mode.")
        }
        AccountError::NotOffline | AccountError::NotMicrosoft => {
            SettingsError::Validation("The selected account does not support this change.")
        }
        AccountError::StaleCapture | AccountError::AlreadyExists => SettingsError::Conflict,
        AccountError::InvalidStoredData => SettingsError::Corrupt,
        AccountError::Storage => SettingsError::Unavailable,
    }
}

pub(super) fn report_save_failure(telemetry: &Telemetry, error: &SettingsError) {
    if matches!(
        error,
        SettingsError::Storage(_) | SettingsError::Unavailable
    ) {
        telemetry.emit(TelemetryEvent::ErrorCaptured {
            kind: TelemetryErrorKind::ConfigSaveFailed,
        });
    }
}

pub(super) fn settings_error(error: SettingsError) -> ApiError {
    let status = match error {
        SettingsError::Validation(_) => StatusCode::BAD_REQUEST,
        SettingsError::Conflict => StatusCode::CONFLICT,
        SettingsError::UnknownFlag => StatusCode::NOT_FOUND,
        SettingsError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        SettingsError::Corrupt | SettingsError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({ "error": error.to_string() })))
}

pub(super) fn json_error(error: JsonRejection) -> ApiError {
    let status = error.status();
    let message = match status {
        StatusCode::BAD_REQUEST => "Invalid JSON syntax.",
        StatusCode::PAYLOAD_TOO_LARGE => "Request body is too large.",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "JSON content type is required.",
        _ => "Invalid JSON request.",
    };
    (status, Json(json!({ "error": message })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        accounts::{microsoft::MinecraftProfile, model::MicrosoftIdentity},
        settings::STATE_INSPECTOR_FLAG,
        storage::{MetadataStore, StorageError},
        telemetry::{CollectorConfig, TelemetryEnvironment},
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use std::time::Duration;
    use tower::ServiceExt;

    struct Fixture {
        metadata: Arc<MetadataStore>,
        settings: Arc<SettingsStore>,
        accounts: Arc<AccountDirectory>,
        telemetry: Arc<Telemetry>,
        tasks: TaskOwner,
        app: Router,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_telemetry(None)
        }

        fn with_telemetry(collector: Option<CollectorConfig>) -> Self {
            let metadata = Arc::new(MetadataStore::in_memory().unwrap());
            let accounts = Arc::new(AccountDirectory::new(metadata.clone()).unwrap());
            let telemetry = Arc::new(Telemetry::new(collector));
            let settings = Arc::new(
                SettingsStore::new_with_telemetry_identity(
                    metadata.clone(),
                    telemetry.export_configured(),
                )
                .unwrap(),
            );
            let tasks = TaskOwner::new(8).unwrap();
            let state = ConfigRouteState::new(
                settings.clone(),
                accounts.clone(),
                telemetry.clone(),
                tasks.clone(),
            )
            .unwrap();
            let app = router(state).merge(super::super::flags::router(
                settings.clone(),
                telemetry.clone(),
                tasks.clone(),
            ));
            Self {
                metadata,
                settings,
                accounts,
                telemetry,
                tasks,
                app,
            }
        }

        async fn request(&self, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
            request(self.app.clone(), method, path, body.to_string()).await
        }

        fn select_microsoft(&self, name: &str) {
            let profile_id = "12345678-1234-4234-8234-123456789abc".to_owned();
            self.accounts
                .commit_microsoft(
                    self.accounts.selection_revision().unwrap(),
                    MicrosoftIdentity {
                        login_id: "23456789-1234-4234-8234-123456789abc".into(),
                        profile_id: profile_id.clone(),
                        display_name: name.into(),
                        credential_revision: 1,
                        profile: MinecraftProfile {
                            id: profile_id,
                            name: name.into(),
                            skins: Vec::new(),
                            capes: Vec::new(),
                        },
                    },
                )
                .unwrap();
        }
    }

    async fn request(app: Router, method: Method, path: &str, body: String) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    const INTERFACE_PATH: &str = "/api/v1/config/interface-preferences";

    fn interface_value() -> Value {
        json!({
            "version":1,
            "preferences":{
                "theme":"nether","customHue":180.0,"customVibrancy":80.0,"lightness":20.0,
                "sounds":false,"hideSkinNametag":true,"selectedSkin":"skin-one",
                "selectedSkinsByAccount":{"account-one":"skin-two"},
                "shortcuts":{"settings":{"key":",","ctrl":true}},
                "overlayPositions":{"status":{"x":12.0,"y":34.0,"scaleX":0.5}},
                "lastUpdateCheckAt":"2026-09-27T12:00:00Z","dismissedUpdateVersion":"1.2.3"
            },
            "route":{"name":"instance","id":"12345678-1234-4234-8234-123456789abc"}
        })
    }

    #[tokio::test]
    async fn interface_preferences_partial_updates_preserve_config_and_other_fields() {
        let collector = CollectorConfig::new(
            "phc_interface_test",
            "http://127.0.0.1:9",
            TelemetryEnvironment::Test,
        )
        .unwrap();
        let fixture = Fixture::with_telemetry(Some(collector));
        fixture.accounts.create_offline_account("Alex").unwrap();
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({
                        "expected_revision":0,"theme":"birch","telemetry_enabled":true
                    })
                )
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    &format!("/api/v1/flags/{STATE_INSPECTOR_FLAG}"),
                    json!({"expected_revision":1,"enabled":true})
                )
                .await
                .0,
            StatusCode::OK
        );
        let config = fixture
            .request(Method::GET, "/api/v1/config", Value::Null)
            .await
            .1;
        let flags = fixture
            .request(Method::GET, "/api/v1/flags", Value::Null)
            .await
            .1;
        let accounts = fixture.accounts.snapshot().unwrap();
        let identity = fixture.settings.telemetry_identity().unwrap();
        let changes = fixture.settings.subscribe().unwrap();
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await,
            (StatusCode::OK, json!({"revision":0,"value":null}))
        );

        let mut value = interface_value();
        let consent = fixture.telemetry.consent_change_owned().await;
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                fixture.request(
                    Method::PUT,
                    INTERFACE_PATH,
                    json!({"expected_revision":0,"change":{"kind":"replace","value":value}})
                )
            )
            .await
            .unwrap(),
            (StatusCode::OK, json!({"revision":1}))
        );
        drop(consent);
        value["route"] = json!({"name":"downloads"});
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    INTERFACE_PATH,
                    json!({
                        "expected_revision":1,"change":{"kind":"route","route":value["route"]}
                    })
                )
                .await,
            (StatusCode::OK, json!({"revision":2}))
        );
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            json!({"revision":2,"value":value})
        );
        value["preferences"]["theme"] = json!("custom");
        assert_eq!(fixture.request(Method::PUT, INTERFACE_PATH, json!({
            "expected_revision":2,"change":{"kind":"local","preferences":value["preferences"]}
        })).await, (StatusCode::OK, json!({"revision":3})));
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            json!({"revision":3,"value":value})
        );
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    INTERFACE_PATH,
                    json!({
                        "expected_revision":3,"change":{"kind":"route","route":null}
                    })
                )
                .await,
            (StatusCode::OK, json!({"revision":4}))
        );
        value["route"] = Value::Null;
        let expected = json!({"revision":4,"value":value});
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    INTERFACE_PATH,
                    json!({
                        "expected_revision":3,"change":{"kind":"replace","value":null}
                    })
                )
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            expected
        );
        assert_eq!(
            fixture
                .request(Method::GET, "/api/v1/config", Value::Null)
                .await
                .1,
            config
        );
        assert_eq!(
            fixture
                .request(Method::GET, "/api/v1/flags", Value::Null)
                .await
                .1,
            flags
        );
        assert_eq!(fixture.accounts.snapshot().unwrap(), accounts);
        assert_eq!(fixture.settings.telemetry_identity().unwrap(), identity);
        assert!(!changes.has_changed().unwrap());
        assert!(fixture.telemetry.emit(TelemetryEvent::AppStarted {
            state_inspector: false
        }));
        assert!(config.get("interface_preferences").is_none());

        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({
                        "expected_revision":config["revision"],"theme":"end"
                    })
                )
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            expected
        );
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    INTERFACE_PATH,
                    json!({
                        "expected_revision":4,"change":{"kind":"replace","value":null}
                    })
                )
                .await,
            (StatusCode::OK, json!({"revision":5}))
        );
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            json!({"revision":5,"value":null})
        );
    }

    #[tokio::test]
    async fn interface_preferences_invalid_requests_and_separate_body_limits_are_atomic() {
        let fixture = Fixture::new();
        for change in [
            json!({"kind":"replace"}),
            json!({"kind":"route"}),
            json!({"kind":"route","route":null,"unknown":true}),
            json!({"kind":"local","preferences":{"shortcuts":{"bad":{"key":"A","ctrl":null}}}}),
        ] {
            assert_eq!(
                fixture
                    .request(
                        Method::PUT,
                        INTERFACE_PATH,
                        json!({"expected_revision":0,"change":change})
                    )
                    .await
                    .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        for change in [
            json!({"kind":"route","route":null}),
            json!({"kind":"local","preferences":{}}),
            json!({"kind":"replace","value":{"version":2,"preferences":{},"route":null}}),
            json!({"kind":"replace","value":{"version":1,"preferences":{"customHue":361},"route":null}}),
        ] {
            assert_eq!(
                fixture
                    .request(
                        Method::PUT,
                        INTERFACE_PATH,
                        json!({"expected_revision":0,"change":change})
                    )
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            json!({"revision":0,"value":null})
        );

        let mut value = interface_value();
        value["preferences"]["selectedSkinsByAccount"] = Value::Object(
            (0..256)
                .map(|index| (format!("account-{index}"), json!("s".repeat(300))))
                .collect(),
        );
        let update = json!({"expected_revision":0,"change":{"kind":"replace","value":value}});
        assert!(update.to_string().len() > 64 * 1024);
        assert_eq!(
            fixture.request(Method::PUT, INTERFACE_PATH, update).await,
            (StatusCode::OK, json!({"revision":1}))
        );
        let expected = json!({"revision":1,"value":value});
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            expected
        );
        for (path, limit) in [
            (INTERFACE_PATH, MAX_INTERFACE_PREFERENCES_BYTES + 1024),
            ("/api/v1/config", 64 * 1024),
        ] {
            let body = format!("{}null", " ".repeat(limit));
            assert_eq!(
                request(fixture.app.clone(), Method::PUT, path, body).await,
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"Request body is too large."})
                )
            );
        }
        assert_eq!(request(fixture.app.clone(), Method::PUT, INTERFACE_PATH,
            r#"{"expected_revision":1,"change":{"kind":"route","route":{"name":"instance","id":"\ud800"}}}"#.into()
        ).await, (StatusCode::BAD_REQUEST, json!({"error":"Invalid JSON syntax."})));

        fixture.metadata.transaction::<_, StorageError>(|transaction| {
            transaction.execute_batch("CREATE TRIGGER reject_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'private-path'); END;")?;
            Ok(())
        }).unwrap();
        let (status, error) = fixture
            .request(
                Method::PUT,
                INTERFACE_PATH,
                json!({
                    "expected_revision":1,"change":{"kind":"replace","value":null}
                }),
            )
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!error.to_string().contains("private-path"));
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            expected
        );
        assert_eq!(fixture.settings.current().unwrap(), ConfigView::default());
    }

    #[tokio::test]
    async fn interface_preferences_commit_survives_a_dropped_http_waiter() {
        let fixture = Fixture::new();
        fixture.accounts.create_offline_account("Alex").unwrap();
        let accounts = fixture.accounts.snapshot().unwrap();
        let config = fixture.settings.current().unwrap();
        let changes = fixture.settings.subscribe().unwrap();
        let metadata = fixture.metadata.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let gate = tokio::task::spawn_blocking(move || {
            metadata
                .read::<_, StorageError>(|_| {
                    entered.send(()).unwrap();
                    held.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .unwrap();
        });
        ready.await.unwrap();
        let app = fixture.app.clone();
        let value = interface_value();
        let update = json!({"expected_revision":0,"change":{"kind":"replace","value":value}});
        let waiter = tokio::spawn(request(
            app,
            Method::PUT,
            INTERFACE_PATH,
            update.to_string(),
        ));
        let mut task_changes = fixture.tasks.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.tasks.status().running.is_empty() {
                task_changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        gate.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.tasks.status().running.is_empty() {
                task_changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            json!({"revision":1,"value":value})
        );
        assert_eq!(fixture.settings.current().unwrap(), config);
        assert_eq!(fixture.accounts.snapshot().unwrap(), accounts);
        assert!(!changes.has_changed().unwrap());
        fixture
            .tasks
            .shutdown(Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    INTERFACE_PATH,
                    json!({
                        "expected_revision":1,"change":{"kind":"replace","value":null}
                    })
                )
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            fixture
                .request(Method::GET, INTERFACE_PATH, Value::Null)
                .await
                .1,
            json!({"revision":1,"value":value})
        );
    }

    #[tokio::test]
    async fn interface_preferences_routes_require_existing_local_authority() {
        use crate::transport::{CAPABILITY_HEADER, LocalApiAuthority, protected_router};

        let fixture = Fixture::new();
        let authority = LocalApiAuthority::new("127.0.0.1:42789".parse().unwrap(), None).unwrap();
        let app = protected_router(fixture.app.clone(), authority.clone());
        for method in [Method::GET, Method::PUT] {
            assert_eq!(
                request(app.clone(), method, INTERFACE_PATH, "{}".into())
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            fixture.settings.interface_preferences().unwrap().revision,
            0
        );
        for (method, body) in [
            (
                Method::PUT,
                json!({"expected_revision":0,"change":{"kind":"replace","value":interface_value()}}),
            ),
            (Method::GET, Value::Null),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(INTERFACE_PATH)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(CAPABILITY_HEADER, authority.capability_for_test())
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(body["revision"], 1);
        }
    }

    #[tokio::test]
    async fn account_projection_is_authoritative_and_setting_rename_commits_together() {
        let fixture = Fixture::new();
        fixture.accounts.create_offline_account("Alex").unwrap();
        let (status, body) = fixture
            .request(Method::GET, "/api/v1/config", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["username"], "Alex");
        assert_eq!(body["launch_auth_mode"], "offline");
        assert_eq!(body["revision"], 0);
        for private in [
            "telemetry_install_id",
            "feature_overrides",
            "library_dir",
            "guardian_mode",
        ] {
            assert!(body.get(private).is_none());
        }
        let (status, saved) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":0,"expected_account_selection_revision":body["account_selection_revision"],"username":"Steve","theme":"birch"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["username"], "Steve");
        assert_eq!(saved["revision"], 1);
        assert_eq!(
            fixture
                .accounts
                .snapshot()
                .unwrap()
                .active_account()
                .unwrap()
                .display_name,
            "Steve"
        );
        let (status, _) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":0,"username":"Other"}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            fixture
                .accounts
                .snapshot()
                .unwrap()
                .active_account()
                .unwrap()
                .display_name,
            "Steve"
        );
        fixture.accounts.create_offline_account("Third").unwrap();
        let (_, body) = fixture
            .request(Method::GET, "/api/v1/config", Value::Null)
            .await;
        assert_eq!(body["username"], "Third");
        let (status, _) = fixture.request(Method::PUT, "/api/v1/config", json!({
            "expected_revision":1,"expected_account_selection_revision":saved["account_selection_revision"],"username":"StaleName"
        })).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            fixture
                .accounts
                .snapshot()
                .unwrap()
                .active_account()
                .unwrap()
                .display_name,
            "Third"
        );
        assert_eq!(fixture.settings.current().unwrap().revision, 1);
    }

    #[tokio::test]
    async fn short_authenticated_names_survive_projection_edits_and_mode_changes() {
        for name in ["A", "A1"] {
            let fixture = Fixture::new();
            fixture.accounts.create_offline_account("Alex").unwrap();
            fixture.select_microsoft(name);
            let account_before = fixture.accounts.snapshot().unwrap();
            let (status, current) = fixture
                .request(Method::GET, "/api/v1/config", Value::Null)
                .await;
            assert_eq!(status, StatusCode::OK, "{current}");
            assert_eq!(current["username"], name);
            assert_eq!(current["launch_auth_mode"], "online");
            let (status, saved) = fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":0,"theme":"birch"}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{saved}");
            assert_eq!(saved["username"], name);
            assert_eq!(saved["launch_auth_mode"], "online");
            assert_eq!(saved["revision"], 1);
            assert_eq!(fixture.accounts.snapshot().unwrap(), account_before);
            let reopened = SettingsStore::new(fixture.metadata.clone()).unwrap();
            assert_eq!(reopened.current().unwrap().username, name);

            // Explicit names are offline edits even while an online account is
            // selected. They must not inherit the provider's shorter-name rule.
            let (status, _) = fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":1,"expected_account_selection_revision":saved["account_selection_revision"],"username":name}),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(fixture.accounts.snapshot().unwrap(), account_before);
            assert_eq!(fixture.settings.current().unwrap().revision, 1);

            let (status, offline) = fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":1,"expected_account_selection_revision":saved["account_selection_revision"],"launch_auth_mode":"offline"}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{offline}");
            assert_eq!(offline["username"], "Alex");
            assert_eq!(offline["launch_auth_mode"], "offline");
            let offline_before = fixture.accounts.snapshot().unwrap();
            let (status, _) = fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":2,"expected_account_selection_revision":offline["account_selection_revision"],"username":name}),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(fixture.accounts.snapshot().unwrap(), offline_before);
            assert_eq!(fixture.settings.current().unwrap().revision, 2);
            let (status, online) = fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":2,"expected_account_selection_revision":offline["account_selection_revision"],"launch_auth_mode":"online"}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{online}");
            assert_eq!(online["username"], name);
            assert_eq!(online["launch_auth_mode"], "online");
        }
    }

    #[tokio::test]
    async fn removing_the_last_short_named_online_account_restores_valid_offline_defaults() {
        let fixture = Fixture::new();
        fixture.select_microsoft("A");
        let (status, _) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":0,"theme":"birch"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let capture = fixture.accounts.capture_selected().unwrap();
        fixture.accounts.remove_microsoft(&capture).unwrap();
        let (status, current) = fixture
            .request(Method::GET, "/api/v1/config", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{current}");
        assert_eq!(current["username"], ConfigView::default().username);
        assert_eq!(current["launch_auth_mode"], "offline");
        let (status, saved) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":1,"theme":"nether"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["username"], ConfigView::default().username);
        assert_eq!(saved["launch_auth_mode"], "offline");
        assert_eq!(saved["revision"], 2);
    }

    #[tokio::test]
    async fn failed_settings_commit_rolls_back_account_rename_and_onboarding() {
        let fixture = Fixture::new();
        fixture.accounts.create_offline_account("Alex").unwrap();
        let before = fixture.accounts.snapshot().unwrap();
        fixture.metadata.transaction(|transaction| -> Result<(), StorageError> {
            transaction.execute_batch("CREATE TRIGGER reject_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'private-path'); END;")?;
            Ok(())
        }).unwrap();
        let (status, error) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":0,"expected_account_selection_revision":before.selection_revision,"username":"Steve"}),
            )
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!error.to_string().contains("private-path"));
        assert_eq!(fixture.accounts.snapshot().unwrap(), before);
        let (status, _) = fixture
            .request(
                Method::POST,
                "/api/v1/onboarding/complete",
                json!({"expected_revision":0}),
            )
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!fixture.settings.current().unwrap().onboarding_done);
        assert_eq!(fixture.settings.current().unwrap().revision, 0);
    }

    #[tokio::test]
    async fn onboarding_and_flag_updates_share_revision_and_nullable_preferences_reset() {
        let fixture = Fixture::new();
        let (status, _) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":0,"custom_hue":12,"music_enabled":false}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, flags) = fixture
            .request(
                Method::PUT,
                &format!("/api/v1/flags/{STATE_INSPECTOR_FLAG}"),
                json!({"expected_revision":1,"enabled":true}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(flags["revision"], 2);
        assert_eq!(flags["flags"][0]["source"], "override");
        let (status, _) = fixture
            .request(
                Method::POST,
                "/api/v1/onboarding/complete",
                json!({"expected_revision":1}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body) = fixture
            .request(
                Method::POST,
                "/api/v1/onboarding/complete",
                json!({"expected_revision":2}),
            )
            .await;
        assert_eq!((status, body), (StatusCode::OK, json!({"status":"ok"})));
        let (status, config) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":3,"custom_hue":null}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(config["custom_hue"], Value::Null);
        assert_eq!(config["music_enabled"], false);
        assert_eq!(config["onboarding_done"], true);
        let (status, flags) = fixture
            .request(
                Method::PUT,
                &format!("/api/v1/flags/{STATE_INSPECTOR_FLAG}"),
                json!({"expected_revision":4,"enabled":null}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(flags["flags"][0]["source"], "default");
        assert_eq!(fixture.settings.current().unwrap().revision, 5);
    }

    #[tokio::test]
    async fn malformed_unknown_missing_and_null_fields_are_bounded_and_do_not_mutate() {
        let fixture = Fixture::new();
        for body in [
            json!({"theme":"nether"}),
            json!({"expected_revision":0,"username":null}),
            json!({"expected_revision":0,"expected_account_selection_revision":null}),
            json!({"expected_revision":0,"guardian_mode":"secret-value"}),
            json!({"expected_revision":0,"library_dir":"/private/secret-value"}),
            json!({"expected_revision":0,"performance_mode":"secret-value"}),
        ] {
            let (status, error) = fixture.request(Method::PUT, "/api/v1/config", body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(error, json!({"error":"Invalid JSON request."}));
        }
        let (status, error) = request(
            fixture.app.clone(),
            Method::PUT,
            "/api/v1/config",
            "{\"username\":\"secret-value".into(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error, json!({"error":"Invalid JSON syntax."}));
        for body in [
            json!({"expected_revision":0,"max_memory_mb":32769}),
            json!({"expected_revision":0,"launch_auth_mode":"online"}),
            json!({"expected_revision":0,"username":"MissingFence"}),
        ] {
            assert_eq!(
                fixture.request(Method::PUT, "/api/v1/config", body).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/onboarding/complete", json!({}))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    &format!("/api/v1/flags/{STATE_INSPECTOR_FLAG}"),
                    json!({"expected_revision":0})
                )
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/flags/unknown",
                    json!({"expected_revision":0,"enabled":true})
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(fixture.settings.current().unwrap(), ConfigView::default());
    }

    #[tokio::test]
    async fn dropped_http_waiter_does_not_cancel_an_accepted_settings_write() {
        let fixture = Fixture::new();
        let mut changes = fixture.settings.subscribe().unwrap();
        let fence = fixture.telemetry.consent_change_owned().await;
        let app = fixture.app.clone();
        let request = tokio::spawn(async move {
            request(
                app,
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":0,"theme":"nether"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.tasks.status().running.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        request.abort();
        drop(fence);
        tokio::time::timeout(Duration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture.settings.current().unwrap().theme,
            axial_app::settings::ConfigTheme::Nether
        );
        fixture
            .tasks
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();
        let (status, _) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({"expected_revision":1,"theme":"birch"}),
            )
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fixture.settings.current().unwrap().revision, 1);
    }

    #[tokio::test]
    async fn consent_is_published_only_after_commit_and_failure_preserves_persisted_consent() {
        let collector = CollectorConfig::new(
            "phc_settings_test",
            "http://127.0.0.1:9",
            TelemetryEnvironment::Test,
        )
        .unwrap();
        let fixture = Fixture::with_telemetry(Some(collector));
        let event = TelemetryEvent::AppStarted {
            state_inspector: false,
        };
        assert!(!fixture.telemetry.emit(event));
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":0,"telemetry_enabled":true})
                )
                .await
                .0,
            StatusCode::OK
        );
        let first_identity = fixture.settings.telemetry_identity().unwrap().unwrap();
        assert!(fixture.telemetry.emit(event));
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":1,"telemetry_enabled":false})
                )
                .await
                .0,
            StatusCode::OK
        );
        assert!(!fixture.telemetry.emit(event));
        assert_eq!(fixture.settings.telemetry_identity().unwrap(), None);
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":2,"telemetry_enabled":true})
                )
                .await
                .0,
            StatusCode::OK
        );
        assert_ne!(
            fixture.settings.telemetry_identity().unwrap().unwrap(),
            first_identity
        );
        fixture.metadata.transaction(|transaction| -> Result<(), StorageError> {
            transaction.execute_batch("CREATE TRIGGER reject_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'failure'); END;")?;
            Ok(())
        }).unwrap();
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":3,"telemetry_enabled":false})
                )
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(fixture.settings.current().unwrap().telemetry_enabled);
        assert!(fixture.telemetry.emit(event));
        assert_eq!(fixture.settings.current().unwrap().revision, 3);
    }

    #[tokio::test]
    async fn failed_opt_out_does_not_export_save_error_but_other_failed_saves_do() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (batches, mut received) = tokio::sync::mpsc::channel::<Value>(2);
        let collector_app = Router::new().route(
            "/batch/",
            post(move |Json(batch): Json<Value>| {
                let batches = batches.clone();
                async move {
                    batches.send(batch).await.unwrap();
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let (shutdown, shutdown_signal) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, collector_app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_signal.await;
                })
                .await
                .unwrap();
        });
        let collector = CollectorConfig::new(
            "phc_settings_test",
            &format!("http://{address}"),
            TelemetryEnvironment::Test,
        )
        .unwrap();
        let fixture = Fixture::with_telemetry(Some(collector));
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":0,"telemetry_enabled":true})
                )
                .await
                .0,
            StatusCode::OK
        );
        fixture.metadata.transaction(|transaction| -> Result<(), StorageError> {
            transaction.execute_batch("CREATE TRIGGER reject_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'private-source'); END;")?;
            Ok(())
        }).unwrap();
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":1,"telemetry_enabled":false})
                )
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fixture.telemetry.flush_once().await, 0);
        assert!(received.try_recv().is_err());
        assert_eq!(
            fixture
                .request(
                    Method::PUT,
                    "/api/v1/config",
                    json!({"expected_revision":1,"theme":"nether"})
                )
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fixture.telemetry.flush_once().await, 1);
        let batch = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            batch["batch"][0]["properties"]["$exception_fingerprint"],
            "config_save_failed"
        );
        assert!(!batch.to_string().contains("private-source"));
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[test]
    fn mismatched_feature_storage_or_exporter_policy_is_rejected() {
        let first = Arc::new(MetadataStore::in_memory().unwrap());
        let second = Arc::new(MetadataStore::in_memory().unwrap());
        let settings = Arc::new(SettingsStore::new(first).unwrap());
        let accounts = Arc::new(AccountDirectory::new(second).unwrap());
        assert!(
            ConfigRouteState::new(
                settings,
                accounts,
                Arc::new(Telemetry::new(None)),
                TaskOwner::new(1).unwrap()
            )
            .is_err()
        );
    }
}
