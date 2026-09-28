use axial_app::{
    accounts::directory::AccountDirectory,
    import::{
        ImportError, ImportPreview, ImportPreviews, MetadataImportError, RulesImportError,
        SkinImportError,
        model::{
            InstanceImportMappings, InstanceImportRequest, InstanceImportResponse,
            MetadataImportRequest, MetadataImportResponse, MetadataImportStatus,
            RulesImportRequest, RulesImportResponse, RulesImportStatus, SkinImportRequest,
            SkinImportResponse, SkinImportStatus,
        },
    },
    instances::{create::InstanceService, model::InstanceError},
    performance::rules::PerformanceRules,
    settings::{SettingsError, SettingsStore},
    skins::library::SavedSkinLibrary,
    tasks::TaskOwner,
    telemetry::Telemetry,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Services {
    previews: Arc<ImportPreviews>,
    instances: Arc<InstanceService>,
    settings: Arc<SettingsStore>,
    accounts: Arc<AccountDirectory>,
    skins: Arc<SavedSkinLibrary>,
    rules: PerformanceRules,
    telemetry: Arc<Telemetry>,
    tasks: TaskOwner,
}
type ApiError = (StatusCode, Json<Value>);

/// Mount within the authenticated exact-origin API. Source admission is native
/// composition work; neither a request path nor stored legacy paths confer it.
pub fn router(
    previews: Arc<ImportPreviews>,
    instances: Arc<InstanceService>,
    settings: Arc<SettingsStore>,
    accounts: Arc<AccountDirectory>,
    skins: Arc<SavedSkinLibrary>,
    rules: PerformanceRules,
    telemetry: Arc<Telemetry>,
    tasks: TaskOwner,
) -> Result<Router, SettingsError> {
    if !std::ptr::eq(instances.registry().storage(), settings.metadata().as_ref())
        || !accounts.uses_metadata(settings.metadata())
        || !rules.uses_metadata(settings.metadata())
        || settings.telemetry_identity_enabled() != telemetry.export_configured()
    {
        return Err(SettingsError::Unavailable);
    }
    Ok(Router::new()
        .route("/api/v1/import/preview", get(preview))
        .route("/api/v1/import/instances", post(import_instance))
        .route(
            "/api/v1/import/instances/{fingerprint}",
            get(instance_mappings),
        )
        .route("/api/v1/import/metadata", post(import_metadata))
        .route("/api/v1/import/metadata/{id}", get(metadata_status))
        .route("/api/v1/import/skins", post(import_skins))
        .route("/api/v1/import/skins/{id}", get(skin_status))
        .route("/api/v1/import/rules", post(import_rules))
        .route("/api/v1/import/rules/{id}", get(rules_status))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(Services {
            previews,
            instances,
            settings,
            accounts,
            skins,
            rules,
            telemetry,
            tasks,
        }))
}

async fn preview(
    State(services): State<Services>,
) -> Result<Json<ImportPreview>, (StatusCode, Json<Value>)> {
    // File identity checks can block, including on an external source volume.
    tokio::task::spawn_blocking(move || services.previews.current_with_rules(&services.rules))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(public_error)
}

async fn import_instance(
    State(services): State<Services>,
    body: Result<Json<InstanceImportRequest>, JsonRejection>,
) -> Result<Json<InstanceImportResponse>, ApiError> {
    let Json(request) = body.map_err(super::config::json_error)?;
    let legacy_id = request.legacy_id.clone();
    let work = tokio::task::spawn_blocking(move || {
        let prepared = services
            .previews
            .prepare_instance_with_rules(&request.fingerprint, &request.legacy_id, &services.rules)
            .map_err(public_error)?;
        services
            .instances
            .import_instance(prepared)
            .map_err(instance_error)
    })
    .await
    .map_err(|_| public_error(ImportError::Unavailable))??;
    // Accepted work retains its own lifetime if the HTTP waiter disappears.
    let instance = work
        .join()
        .await
        .map_err(|_| instance_error(InstanceError::SettlementRequired))?
        .map_err(instance_error)?;
    Ok(Json(InstanceImportResponse {
        legacy_id,
        instance,
        cutover_available: false,
    }))
}

async fn instance_mappings(
    State(services): State<Services>,
    Path(fingerprint): Path<String>,
) -> Result<Json<InstanceImportMappings>, ApiError> {
    tokio::task::spawn_blocking(move || {
        services
            .previews
            .instance_mappings(&fingerprint, &services.instances)
    })
    .await
    .map_err(|_| public_error(ImportError::Unavailable))?
    .map(Json)
    .map_err(public_error)
}

async fn metadata_status(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> Result<Json<MetadataImportStatus>, ApiError> {
    tokio::task::spawn_blocking(move || axial_app::import::metadata_status(&services.settings, &id))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(metadata_error)
}

async fn import_metadata(
    State(services): State<Services>,
    body: Result<Json<MetadataImportRequest>, JsonRejection>,
) -> Result<Json<MetadataImportResponse>, ApiError> {
    let Json(request) = body.map_err(super::config::json_error)?;
    let previews = services.previews.clone();
    let fingerprint = request.fingerprint.clone();
    let prepared = tokio::task::spawn_blocking(move || previews.prepare_metadata(&fingerprint))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map_err(public_error)?;
    let root = services
        .instances
        .directories()
        .library()
        .admit_application_root()
        .map_err(|_| public_error(ImportError::Unavailable))?;
    let tasks = services.tasks.clone();
    let work = tasks
        .try_spawn(root.clone(), move |cancellation| async move {
            let consent = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(MetadataImportError::Source(ImportError::Cancelled)),
                consent = services.telemetry.consent_change_owned() => consent,
            };
            if cancellation.is_cancelled() {
                return Err(MetadataImportError::Source(ImportError::Cancelled));
            }
            // The accepted task retains source, root and consent admission
            // through commit and publication if its HTTP waiter disappears.
            tokio::task::spawn_blocking(move || {
                let commit = prepared.commit(
                    &services.settings,
                    &services.accounts,
                    &root,
                    &request,
                    &cancellation,
                )?;
                consent.publish(
                    commit.settings.config.telemetry_enabled,
                    commit.settings.telemetry_identity.as_deref(),
                );
                Ok(commit.response)
            })
            .await
            .expect("metadata import persistence task panicked")
        })
        .map_err(|_| public_error(ImportError::Unavailable))?;
    work.join()
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(metadata_error)
}

fn metadata_error(error: MetadataImportError) -> ApiError {
    match error {
        MetadataImportError::Source(error) => public_error(error),
        MetadataImportError::Settings(error) => super::config::settings_error(error),
    }
}

async fn skin_status(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> Result<Json<SkinImportStatus>, ApiError> {
    tokio::task::spawn_blocking(move || axial_app::import::skin_status(&services.skins, &id))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(skin_error)
}

async fn import_skins(
    State(services): State<Services>,
    body: Result<Json<SkinImportRequest>, JsonRejection>,
) -> Result<Json<SkinImportResponse>, ApiError> {
    let Json(request) = body.map_err(super::config::json_error)?;
    let previews = services.previews.clone();
    let fingerprint = request.fingerprint.clone();
    let prepared = tokio::task::spawn_blocking(move || previews.prepare_skins(&fingerprint))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map_err(public_error)?;
    let root = services
        .instances
        .directories()
        .library()
        .admit_application_root()
        .map_err(|_| public_error(ImportError::Unavailable))?;
    let tasks = services.tasks.clone();
    let work = tasks
        .try_spawn(root, move |cancellation| async move {
            tokio::task::spawn_blocking(move || {
                prepared.commit(&services.skins, &request, &cancellation)
            })
            .await
            .expect("saved skin import persistence task panicked")
        })
        .map_err(|_| public_error(ImportError::Unavailable))?;
    work.join()
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(skin_error)
}

fn skin_error(error: SkinImportError) -> ApiError {
    match error {
        SkinImportError::Source(error) => public_error(error),
        SkinImportError::Library(error) => super::skin::library_error(error),
    }
}

async fn rules_status(
    State(services): State<Services>,
    Path(id): Path<String>,
) -> Result<Json<RulesImportStatus>, ApiError> {
    tokio::task::spawn_blocking(move || axial_app::import::rules_status(&services.rules, &id))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(rules_error)
}

async fn import_rules(
    State(services): State<Services>,
    body: Result<Json<RulesImportRequest>, JsonRejection>,
) -> Result<Json<RulesImportResponse>, ApiError> {
    let Json(request) = body.map_err(super::config::json_error)?;
    let previews = services.previews.clone();
    let fingerprint = request.fingerprint.clone();
    let prepared = tokio::task::spawn_blocking(move || previews.prepare_rules(&fingerprint))
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map_err(public_error)?;
    let root = services
        .instances
        .directories()
        .library()
        .admit_application_root()
        .map_err(|_| public_error(ImportError::Unavailable))?;
    let tasks = services.tasks.clone();
    let work = tasks
        .try_spawn(root.clone(), move |cancellation| async move {
            prepared
                .commit(&services.rules, &root, &request, &cancellation)
                .await
        })
        .map_err(|_| public_error(ImportError::Unavailable))?;
    work.join()
        .await
        .map_err(|_| public_error(ImportError::Unavailable))?
        .map(Json)
        .map_err(rules_error)
}

fn rules_error(error: RulesImportError) -> ApiError {
    let error = match error {
        RulesImportError::Source(error) => return public_error(error),
        RulesImportError::Rules(error) => error,
    };
    use axial_app::performance::rules::RulesImportError as Error;
    let status = match error {
        Error::Invalid | Error::Unconfigured | Error::Untrusted => StatusCode::UNPROCESSABLE_ENTITY,
        Error::Conflict => StatusCode::CONFLICT,
        Error::Unavailable | Error::Cancelled => StatusCode::SERVICE_UNAVAILABLE,
        Error::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // The rules owner provides fixed public text, including storage failures.
    (status, Json(json!({ "error": error.to_string() })))
}

fn public_error(error: ImportError) -> ApiError {
    let (status, message) = match error {
        ImportError::NoSource => (
            StatusCode::NOT_FOUND,
            "No predecessor profile has been admitted for preview.",
        ),
        ImportError::SourceChanged => (
            StatusCode::CONFLICT,
            "The predecessor profile changed. Create a new preview.",
        ),
        ImportError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Import is temporarily unavailable.",
        ),
        ImportError::Cancelled => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The import was cancelled before publication.",
        ),
        ImportError::InvalidData => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "The predecessor data requires a conversion that is not available yet.",
        ),
        ImportError::LimitExceeded => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "The predecessor profile exceeds the import limits.",
        ),
        _ => (
            StatusCode::CONFLICT,
            "The predecessor profile could not be safely previewed.",
        ),
    };
    // Never echo source IO errors, absolute paths, credentials or journal text.
    (status, Json(json!({ "error": message })))
}

fn instance_error(error: InstanceError) -> ApiError {
    let status = match error {
        InstanceError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        InstanceError::Closed => StatusCode::SERVICE_UNAVAILABLE,
        InstanceError::NotFound => StatusCode::NOT_FOUND,
        InstanceError::InvalidInput | InstanceError::InvalidSettings => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        _ => StatusCode::CONFLICT,
    };
    // InstanceError owns bounded, fixed public text, including storage failures.
    (status, Json(json!({ "error": error.to_string() })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        accounts::{
            credential_store::CredentialStore,
            model::{AccountKind, LaunchAuthMode},
            session::{AuthError, AuthService},
        },
        import::{Inventory, ReadOnlySource},
        instances::directory::{InstanceDirectories, Registry},
        library::{LibraryId, LibraryLifecycle},
        skins::{
            library::{SavedSkinDeleteResult, UpdateSavedSkinRequest},
            store::SavedSkinStore,
        },
        storage::{MetadataStore, StorageError},
        tasks::{Exclusions, TaskOwner},
        telemetry::{CollectorConfig, TelemetryEnvironment, TelemetryEvent},
    };
    use axial_fs::{RootSession, RootSessionAcquireOutcome};
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };
    use tower::ServiceExt;

    const FIRST: &str = "0000000000000001";
    const LEGACY_MICROSOFT_ID: &str = "microsoft-msa-1111111111111111-2222222222222222";
    const LEGACY_LOGIN_ID: &str = "msa-3333333333333333-4444444444444444";
    const MICROSOFT_PROFILE: &str = "12345678123442348234123456789abc";
    const MICROSOFT_ID: &str = "12345678-1234-4234-8234-123456789abc";

    struct Fixture {
        _root: tempfile::TempDir,
        baseline: PathBuf,
        source: ReadOnlySource,
        services: Services,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let baseline = root.path().join("baseline");
            let replacement = root.path().join("replacement");
            fs::create_dir(&baseline).unwrap();
            fs::create_dir(&replacement).unwrap();
            copy_fixture(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../acceptance/fixtures/profiles/offline-vanilla"),
                &baseline,
            );
            let session = match RootSession::acquire(&replacement) {
                RootSessionAcquireOutcome::Acquired(session) => session,
                other => panic!("isolated replacement: {other:?}"),
            };
            let source = ReadOnlySource::from_admitted_directory(
                session.admit_absolute_directory(&baseline).unwrap(),
            );
            let inventory = Inventory::capture(&source, &BTreeMap::new()).unwrap();
            let library = LibraryLifecycle::from_root_session_at(
                session,
                LibraryId::new(),
                replacement.clone(),
            )
            .unwrap();
            let storage =
                Arc::new(MetadataStore::open(replacement.join("metadata.sqlite")).unwrap());
            storage
                .migrate(&[
                    axial_app::instances::directory::MIGRATION,
                    axial_app::instances::create::MIGRATION,
                    axial_app::instances::create::DUPLICATE_WITNESS_MIGRATION,
                    axial_app::instances::import::MIGRATION,
                    axial_app::settings::SETTINGS_MIGRATION,
                    axial_app::import::METADATA_IMPORT_MIGRATION,
                    axial_app::import::METADATA_IMPORT_IDENTITIES_MIGRATION,
                    axial_app::performance::rules::MIGRATION,
                    axial_app::performance::rules::IMPORT_MIGRATION,
                ])
                .unwrap();
            let telemetry = Arc::new(Telemetry::new(Some(
                CollectorConfig::new(
                    "phc_import_fixture",
                    "http://127.0.0.1:9",
                    TelemetryEnvironment::Test,
                )
                .unwrap(),
            )));
            let settings = Arc::new(
                SettingsStore::new_with_telemetry_identity(storage.clone(), true).unwrap(),
            );
            let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
            storage
                .migrate(&[
                    axial_app::skins::store::MIGRATION,
                    axial_app::skins::store::IMPORT_MIGRATION,
                ])
                .unwrap();
            let skins = Arc::new(SavedSkinLibrary::new(
                SavedSkinStore::new(storage.clone()),
                library.admit_application_root().unwrap(),
            ));
            let tasks = TaskOwner::new(4).unwrap();
            let rules = PerformanceRules::with_remote(storage.clone(), None, None).unwrap();
            let instances = Arc::new(InstanceService::new(
                InstanceDirectories::new(Registry::new(storage), library, Exclusions::new()),
                tasks.clone(),
            ));
            let previews = Arc::new(ImportPreviews::new());
            previews.admit(inventory).unwrap();
            Self {
                _root: root,
                baseline,
                source,
                services: Services {
                    previews,
                    instances,
                    settings,
                    accounts,
                    skins,
                    rules,
                    telemetry,
                    tasks,
                },
            }
        }

        fn router(&self) -> Router {
            router(
                self.services.previews.clone(),
                self.services.instances.clone(),
                self.services.settings.clone(),
                self.services.accounts.clone(),
                self.services.skins.clone(),
                self.services.rules.clone(),
                self.services.telemetry.clone(),
                self.services.tasks.clone(),
            )
            .unwrap()
            .merge(super::super::config::router(
                super::super::config::ConfigRouteState::new(
                    self.services.settings.clone(),
                    self.services.accounts.clone(),
                    self.services.telemetry.clone(),
                    self.services.tasks.clone(),
                )
                .unwrap(),
            ))
        }

        async fn post(&self, body: Value) -> (StatusCode, Value) {
            self.request(Method::POST, "/api/v1/import/instances", body)
                .await
        }

        async fn request(&self, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
            self.request_body(method, path, body.to_string()).await
        }

        async fn request_body(
            &self,
            method: Method,
            path: &str,
            body: String,
        ) -> (StatusCode, Value) {
            let response = self
                .router()
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
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        }

        fn trust_rules(&mut self, key: Option<String>) {
            self.services.rules = PerformanceRules::with_remote(
                self.services.settings.metadata().clone(),
                key.as_ref().map(|_| "https://example.invalid/rules".into()),
                key,
            )
            .unwrap();
        }

        fn admit_source_rules(&mut self, time: &str) -> (Value, Vec<u8>) {
            let (bytes, key) = signed_rules_cache(time);
            fs::create_dir_all(self.baseline.join("performance")).unwrap();
            fs::write(self.baseline.join("performance/rules-cache.json"), &bytes).unwrap();
            self.trust_rules(Some(key));
            let preview = self
                .services
                .previews
                .admit(Inventory::capture(&self.source, &BTreeMap::new()).unwrap())
                .unwrap();
            assert!(preview.rules_import_available);
            (
                json!({"rules_import_id":preview.rules_import_id,"fingerprint":preview.fingerprint}),
                bytes,
            )
        }

        fn stored_rules_cache(&self) -> Option<Vec<u8>> {
            use axial_app::storage::rusqlite::OptionalExtension;
            self.services
                .settings
                .metadata()
                .read(|db| {
                    db.query_row(
                        "SELECT snapshot FROM performance_rules WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(StorageError::from)
                })
                .unwrap()
        }

        fn metadata_request(&self) -> Value {
            let preview = self.services.previews.current().unwrap();
            json!({
                "metadata_import_id":preview.metadata_import_id,
                "fingerprint":preview.fingerprint,
                "expected_settings_revision":self.services.settings.current().unwrap().revision,
                "expected_account_selection_revision":self.services.accounts.selection_revision().unwrap(),
            })
        }

        fn admit_source_skins(&self, skins: &[(Value, Vec<u8>)]) -> Value {
            let directory = self.baseline.join("skins");
            fs::create_dir_all(directory.join("files")).unwrap();
            let records: Vec<_> = skins.iter().map(|(record, _)| record).collect();
            fs::write(
                directory.join("index.json"),
                serde_json::to_vec(&json!({
                    "schema":"axial.skins.saved", "schema_version":3, "skins":records
                }))
                .unwrap(),
            )
            .unwrap();
            for (record, png) in skins {
                fs::write(
                    directory.join(format!(
                        "files/{}.png",
                        record["texture_key"].as_str().unwrap()
                    )),
                    png,
                )
                .unwrap();
            }
            let preview = self
                .services
                .previews
                .admit(Inventory::capture(&self.source, &BTreeMap::new()).unwrap())
                .unwrap();
            assert!(preview.skin_import_available);
            json!({"skin_import_id":preview.skin_import_id,"fingerprint":preview.fingerprint})
        }

        fn enable_source_telemetry(&self) {
            let path = self.baseline.join("config.json");
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config["telemetry_enabled"] = json!(true);
            config["telemetry_install_id"] = json!("predecessor-secret-identity");
            fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
            self.services
                .previews
                .admit(Inventory::capture(&self.source, &BTreeMap::new()).unwrap())
                .unwrap();
        }

        fn select_source_microsoft(&self, keep_offline: bool) {
            let path = self.baseline.join("accounts.json");
            let mut accounts: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let rows = accounts["accounts"].as_array_mut().unwrap();
            if !keep_offline {
                rows.clear();
            }
            rows.push(json!({
                "account_id":LEGACY_MICROSOFT_ID, "kind":"microsoft", "display_name":"A",
                "login_id":LEGACY_LOGIN_ID, "minecraft_profile_id":MICROSOFT_PROFILE,
                "created_at":"2026-09-08T00:02:00Z", "updated_at":"2026-09-08T00:03:00Z"
            }));
            accounts["active_account_id"] = json!(LEGACY_MICROSOFT_ID);
            fs::write(path, serde_json::to_vec(&accounts).unwrap()).unwrap();
            let path = self.baseline.join("config.json");
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config["username"] = json!("A");
            config["launch_auth_mode"] = json!("online");
            fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
            self.services
                .previews
                .admit(Inventory::capture(&self.source, &BTreeMap::new()).unwrap())
                .unwrap();
        }
    }

    async fn read_route(app: Router, path: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn copy_fixture(source: &Path, destination: &Path) {
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let path = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                fs::create_dir(&path).unwrap();
                copy_fixture(&entry.path(), &path);
            } else {
                fs::copy(entry.path(), path).unwrap();
            }
        }
    }

    fn saved_skin(red: u8, name: &str) -> (Value, Vec<u8>) {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[red, 20, 30, 255].repeat(64 * 64))
                .unwrap();
        }
        let png = axial_app::media::normalize_skin_png(&bytes)
            .unwrap()
            .png_bytes;
        let record = json!({
            "texture_key":axial_app::media::texture_key(&png), "name":name,
            "variant":"slim", "source":"minecraft_username_skin", "cape_id":"legacy-cape",
            "created_at":"2026-09-08T00:00:00Z", "updated_at":"2026-09-08T00:01:00Z",
            "applied_at":"2026-09-08T00:02:00Z", "byte_size":png.len()
        });
        (record, png)
    }

    fn signed_rules_cache(time: &str) -> (Vec<u8>, String) {
        use axial_performance::{RuleChannel, RuleSource, RulesValidation};
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[23; 32]);
        let mut manifest = axial_performance::builtin_manifest().unwrap();
        manifest.generated_at = time.into();
        let signature =
            key.sign(&axial_performance::canonical_manifest_payload(&manifest).unwrap());
        let cache = axial_performance::RulesCacheSnapshot {
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            schema_version: manifest.schema_version,
            generated_at: time.into(),
            validation: RulesValidation::Valid,
            updated_at: time.into(),
            manifest,
            signature: axial_performance::RulesSignatureMetadata {
                signature: hex::encode(signature.to_bytes()),
                key_id: Some("retained-fixture".into()),
            },
        };
        (
            cache.encode().unwrap(),
            hex::encode(key.verifying_key().to_bytes()),
        )
    }

    fn rules_history(fixture: &Fixture) -> Value {
        let cache = json!({"system":"Performance","kind":"Config","id":"performance_rules_cache","ownership":"LauncherManaged"});
        let mut expected = Vec::new();
        let entries: Vec<_> = [7, 9_007_199_254_740_993, u64::MAX - 1].into_iter().enumerate().map(|(index, sequence)| {
            let operation = format!("op-{}", uuid::Uuid::new_v4());
            let success = index == 0;
            let failure = match index {
                1 => json!("refresh_remote_rules"),
                2 => json!("refresh_rules_journal_reconciliation"),
                _ => Value::Null,
            };
            expected.push(json!({"operation_id":operation,"sequence":sequence.to_string(),"outcome":if success {
                json!({"state":"succeeded","cache_changed":true})
            } else {
                json!({"state":"failed","failure_point":failure})
            }}));
            let step = |result: &str, changed: bool| json!({"step_id":"refresh_remote_rules","phase":"Running","result":result,"changed_target":if changed {cache.clone()} else {Value::Null},"generated_facts":[],"rollback":"NotApplicable","guardian_fact_ids":[],"metrics":null});
            json!({"journal_id":format!("journal-{operation}"),"operation_id":operation,"sequence":sequence,"parent_operation_id":null,"command":"RefreshPerformanceRules","intent":{"kind":"generic"},"status":if success {"Succeeded"} else {"Failed"},"owner":"Application","ownership":"LauncherManaged",
                "targets":[{"system":"Performance","kind":"NetworkResource","id":"performance_rules_remote_source","ownership":"ExternalProviderDerived"},cache],
                "planned_steps":[step("Planned",false)],"completed_steps":[step(if success {"Completed"} else {"Failed"},success)],"failure_point":failure,"rollback":"NotApplicable","guardian_diagnosis_ids":[],"outcome":if success {"Succeeded"} else {"Failed"},"reconciliation_attempt":null,"reconciliation_terminal":null,"persisted_state_repair_attempt":null,"persisted_state_repair_terminal":null,"guardian_install_terminal":null})
        }).collect();
        fs::create_dir_all(fixture.baseline.join("state")).unwrap();
        fs::write(
            fixture.baseline.join("state/operation-journals.json"),
            serde_json::to_vec(&json!({"schema":"axial.state.operation_journals.v10","next_sequence":u64::MAX,"entries":entries})).unwrap(),
        ).unwrap();
        json!(expected)
    }

    #[tokio::test]
    async fn rules_import_routes_preserve_signed_bytes_history_and_receipt_aware_instances() {
        let mut fixture = Fixture::new();
        let history = rules_history(&fixture);
        let (request, cache) = fixture.admit_source_rules("2001-01-01T00:00:00Z");
        let status_path = format!(
            "/api/v1/import/rules/{}",
            request["rules_import_id"].as_str().unwrap()
        );
        let (status, before) = read_route(fixture.router(), &status_path).await;
        assert_eq!(status, StatusCode::OK, "{before}");
        assert_eq!(
            before,
            json!({"receipt":null,"stored_cache_matches_import":false,"cutover_available":false})
        );
        let (_, preview) = read_route(fixture.router(), "/api/v1/import/preview").await;
        assert_eq!(preview["instances"][0]["ordinary_import_available"], false);
        let instance_request = json!({"fingerprint":request["fingerprint"],"legacy_id":FIRST});
        assert_eq!(
            fixture.post(instance_request.clone()).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let (status, imported) = fixture
            .request(Method::POST, "/api/v1/import/rules", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        assert_eq!(
            imported["receipt"]["rules_import_id"],
            request["rules_import_id"]
        );
        assert_eq!(imported["receipt"]["fingerprint"], request["fingerprint"]);
        assert_eq!(imported["receipt"]["refresh_history"], history);
        assert_eq!(
            imported["receipt"]["cache_sha256"].as_str().unwrap().len(),
            64
        );
        assert_eq!(imported["already_imported"], false);
        assert_eq!(imported["stored_cache_matches_import"], true);
        assert_eq!(imported["cutover_available"], false);
        assert_eq!(fixture.stored_rules_cache(), Some(cache.clone()));
        assert_eq!(
            fs::read(fixture.baseline.join("performance/rules-cache.json")).unwrap(),
            cache
        );

        let (status, replay) = fixture
            .request(Method::POST, "/api/v1/import/rules", request)
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        let mut expected_replay = imported.clone();
        expected_replay["already_imported"] = json!(true);
        assert_eq!(replay, expected_replay);
        let (status, preview) = read_route(fixture.router(), "/api/v1/import/preview").await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        assert_eq!(preview["instances"][0]["ordinary_import_available"], true);
        let (status, instance) = fixture.post(instance_request).await;
        assert_eq!(status, StatusCode::OK, "{instance}");

        fixture.services.previews.forget().unwrap();
        fs::rename(
            &fixture.baseline,
            fixture._root.path().join("source-offline"),
        )
        .unwrap();
        fixture.trust_rules(None);
        let (status, recorded) = read_route(fixture.router(), &status_path).await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(
            recorded,
            json!({"receipt":imported["receipt"],"stored_cache_matches_import":true,"cutover_available":false})
        );
    }

    #[tokio::test]
    async fn rules_import_refuses_missing_destination_trust_and_wrong_signing_key_without_effects()
    {
        let remote_url = Some("https://example.invalid/rules".to_owned());
        let (_, valid_key) = signed_rules_cache("2001-01-01T00:00:00Z");
        let wrong_key = ed25519_dalek::SigningKey::from_bytes(&[24; 32]);
        for (url, key, configured) in [
            (None, None, false),
            (None, Some(valid_key), false),
            (remote_url.clone(), None, false),
            (
                remote_url,
                Some(hex::encode(wrong_key.verifying_key().to_bytes())),
                true,
            ),
        ] {
            let mut fixture = Fixture::new();
            let (request, cache) = fixture.admit_source_rules("2001-01-01T00:00:00Z");
            fixture.services.rules = PerformanceRules::with_remote(
                fixture.services.settings.metadata().clone(),
                url,
                key,
            )
            .unwrap();
            let (status, error) = fixture
                .request(Method::POST, "/api/v1/import/rules", request.clone())
                .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
            assert_eq!(
                error,
                json!({"error":if configured {
                    "The predecessor rules are not trusted by this destination."
                } else {
                    "The destination rules signing key and remote policy must be configured."
                }})
            );
            assert_eq!(fixture.stored_rules_cache(), None);
            let (_, recorded) = read_route(
                fixture.router(),
                &format!(
                    "/api/v1/import/rules/{}",
                    request["rules_import_id"].as_str().unwrap()
                ),
            )
            .await;
            assert!(recorded["receipt"].is_null());
            assert_eq!(
                fs::read(fixture.baseline.join("performance/rules-cache.json")).unwrap(),
                cache
            );
        }
    }

    #[tokio::test]
    async fn rules_import_conflicts_preserve_destination_and_historical_receipt() {
        let mut fixture = Fixture::new();
        let (first_request, first_cache) = fixture.admit_source_rules("2001-01-01T00:00:00Z");
        let (status, first) = fixture
            .request(Method::POST, "/api/v1/import/rules", first_request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let (next_request, next_cache) = fixture.admit_source_rules("2002-01-01T00:00:00Z");
        let (status, error) = fixture
            .request(Method::POST, "/api/v1/import/rules", next_request.clone())
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert_eq!(fixture.stored_rules_cache(), Some(first_cache));
        let (_, missing) = read_route(
            fixture.router(),
            &format!(
                "/api/v1/import/rules/{}",
                next_request["rules_import_id"].as_str().unwrap()
            ),
        )
        .await;
        assert!(missing["receipt"].is_null());

        // Model a later persisted cache; historical completion stays immutable.
        fixture
            .services
            .settings
            .metadata()
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute(
                    "UPDATE performance_rules SET snapshot=?1 WHERE singleton=1",
                    [&next_cache],
                )?;
                Ok(())
            })
            .unwrap();
        fixture.services.previews.forget().unwrap();
        let (status, recorded) = read_route(
            fixture.router(),
            &format!(
                "/api/v1/import/rules/{}",
                first_request["rules_import_id"].as_str().unwrap()
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(
            recorded,
            json!({"receipt":first["receipt"],"stored_cache_matches_import":false,"cutover_available":false})
        );
    }

    #[tokio::test]
    async fn dropped_rules_import_waiter_commits_after_preview_forget() {
        let mut fixture = Fixture::new();
        let (request, cache) = fixture.admit_source_rules("2001-01-01T00:00:00Z");
        let status_path = format!(
            "/api/v1/import/rules/{}",
            request["rules_import_id"].as_str().unwrap()
        );
        let lease = fixture
            .services
            .rules
            .plan(axial_performance::ResolutionRequest {
                game_version: "1.20.1".into(),
                loader: "vanilla".into(),
                mode: axial_performance::PerformanceMode::Vanilla,
                hardware: Default::default(),
                installed_mods: Vec::new(),
            })
            .await
            .unwrap();
        let app = fixture.router();
        let waiter = tokio::spawn(async move {
            app.oneshot(
                Request::post("/api/v1/import/rules")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
        });
        let mut changed = fixture.services.tasks.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.services.tasks.status().running.is_empty() {
                changed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        fixture.services.previews.forget().unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(fixture.stored_rules_cache(), None);
        drop(lease);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.services.tasks.status().is_idle() {
                changed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let (status, recorded) = read_route(fixture.router(), &status_path).await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert!(recorded["receipt"].is_object());
        assert_eq!(recorded["stored_cache_matches_import"], true);
        assert_eq!(recorded["cutover_available"], false);
        assert_eq!(fixture.stored_rules_cache(), Some(cache));
        fixture
            .services
            .tasks
            .shutdown(Duration::from_secs(3))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rules_import_rejects_untrusted_transport_malformed_bodies_and_stale_admission() {
        let mut fixture = Fixture::new();
        let (request, _) = fixture.admit_source_rules("2001-01-01T00:00:00Z");
        let authority =
            crate::transport::LocalApiAuthority::new("127.0.0.1:12345".parse().unwrap(), None)
                .unwrap();
        let app = fixture.router().layer(axum::middleware::from_fn_with_state(
            authority.clone(),
            crate::transport::authenticate_request,
        ));
        let status_path = format!(
            "/api/v1/import/rules/{}",
            request["rules_import_id"].as_str().unwrap()
        );
        for (method, path) in [
            (Method::POST, "/api/v1/import/rules"),
            (Method::GET, status_path.as_str()),
        ] {
            for foreign_origin in [false, true] {
                let mut builder = Request::builder()
                    .method(method.clone())
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json");
                if foreign_origin {
                    builder = builder
                        .header(
                            crate::transport::CAPABILITY_HEADER,
                            authority.capability_for_test(),
                        )
                        .header(header::ORIGIN, "https://untrusted.invalid");
                }
                let response = app
                    .clone()
                    .oneshot(builder.body(Body::from(request.to_string())).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    if foreign_origin {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
            }
        }
        for (body, expected) in [
            ("{".into(), StatusCode::BAD_REQUEST),
            (json!({"fingerprint":request["fingerprint"],"rules_import_id":request["rules_import_id"],"source_path":"/private/signing-secret"}).to_string(), StatusCode::UNPROCESSABLE_ENTITY),
            (format!("{{\"fingerprint\":{},\"rules_import_id\":{},\"fingerprint\":{}}}", request["fingerprint"], request["rules_import_id"], request["fingerprint"]), StatusCode::UNPROCESSABLE_ENTITY),
            (json!({"fingerprint":"x".repeat(4096),"rules_import_id":request["rules_import_id"]}).to_string(), StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let (status, error) = fixture.request_body(Method::POST, "/api/v1/import/rules", body).await;
            assert_eq!(status, expected, "{error}");
            assert!(!error.to_string().contains("signing-secret"));
        }
        for field in ["fingerprint", "rules_import_id"] {
            let mut stale = request.clone();
            stale[field] = json!("0".repeat(64));
            assert_eq!(
                fixture
                    .request(Method::POST, "/api/v1/import/rules", stale)
                    .await
                    .0,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            read_route(fixture.router(), "/api/v1/import/rules/not-an-id")
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        fs::write(
            fixture.baseline.join("performance/rules-cache.json"),
            b"changed source canary",
        )
        .unwrap();
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/import/rules", request)
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(fixture.stored_rules_cache(), None);
        let (_, recorded) = read_route(fixture.router(), &status_path).await;
        assert!(recorded["receipt"].is_null());
    }

    #[tokio::test]
    async fn rules_import_history_only_serializes_absent_cache_and_safe_storage_failure() {
        let fixture = Fixture::new();
        let history = rules_history(&fixture);
        let preview = fixture
            .services
            .previews
            .admit(Inventory::capture(&fixture.source, &BTreeMap::new()).unwrap())
            .unwrap();
        let request =
            json!({"fingerprint":preview.fingerprint,"rules_import_id":preview.rules_import_id});
        fixture.services.settings.metadata().transaction(|tx| -> Result<_, StorageError> {
            tx.execute_batch("CREATE TRIGGER reject_rules_import BEFORE INSERT ON performance_rules_imports BEGIN SELECT RAISE(ABORT, 'private storage canary'); END;")?;
            Ok(())
        }).unwrap();
        let (status, error) = fixture
            .request(Method::POST, "/api/v1/import/rules", request.clone())
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
        assert_eq!(
            error,
            json!({"error":"Rules import storage is unavailable."})
        );
        let status_path = format!(
            "/api/v1/import/rules/{}",
            request["rules_import_id"].as_str().unwrap()
        );
        assert!(read_route(fixture.router(), &status_path).await.1["receipt"].is_null());
        fixture
            .services
            .settings
            .metadata()
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute_batch("DROP TRIGGER reject_rules_import")?;
                Ok(())
            })
            .unwrap();
        let (status, imported) = fixture
            .request(Method::POST, "/api/v1/import/rules", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        assert_eq!(
            imported,
            json!({"receipt":{"rules_import_id":request["rules_import_id"],"fingerprint":request["fingerprint"],"cache_sha256":null,"refresh_history":history},"already_imported":false,"stored_cache_matches_import":false,"cutover_available":false})
        );
        assert_eq!(fixture.stored_rules_cache(), None);
    }

    #[test]
    fn rules_import_router_rejects_a_different_metadata_owner() {
        let fixture = Fixture::new();
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                axial_app::performance::rules::MIGRATION,
                axial_app::performance::rules::IMPORT_MIGRATION,
            ])
            .unwrap();
        assert!(
            router(
                fixture.services.previews.clone(),
                fixture.services.instances.clone(),
                fixture.services.settings.clone(),
                fixture.services.accounts.clone(),
                fixture.services.skins.clone(),
                PerformanceRules::with_remote(storage, None, None).unwrap(),
                fixture.services.telemetry.clone(),
                fixture.services.tasks.clone(),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn saved_skin_import_routes_preserve_bytes_metadata_and_completed_receipt() {
        let mut fixture = Fixture::new();
        fixture
            .services
            .accounts
            .create_offline_account("SkinOwner")
            .unwrap();
        let skins = [
            saved_skin(11, "First saved skin"),
            saved_skin(22, "Second saved skin"),
        ];
        let request = fixture.admit_source_skins(&skins);
        let status_path = format!(
            "/api/v1/import/skins/{}",
            request["skin_import_id"].as_str().unwrap()
        );
        let source_index = fs::read(fixture.baseline.join("skins/index.json")).unwrap();
        let settings = fixture.services.settings.current().unwrap();
        let accounts = fixture.services.accounts.snapshot().unwrap();
        let (status, absent) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{absent}");
        assert_eq!(absent, json!({"receipt":null,"cutover_available":false}));
        let (status, imported) = fixture
            .request(Method::POST, "/api/v1/import/skins", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        assert_eq!(imported["already_imported"], false);
        assert_eq!(imported["cutover_available"], false);
        let mut keys: Vec<_> = skins
            .iter()
            .map(|(record, _)| record["texture_key"].as_str().unwrap())
            .collect();
        keys.sort();
        assert_eq!(
            imported["receipt"],
            json!({"skin_import_id":request["skin_import_id"],"fingerprint":request["fingerprint"],"texture_keys":keys})
        );
        let auth = Arc::new(AuthService::new(
            fixture.services.accounts.clone(),
            Arc::new(CredentialStore::with_task_owner(
                uuid::Uuid::new_v4(),
                fixture.services.tasks.clone(),
            )),
            fixture.services.tasks.clone(),
        ));
        let media = axial_app::skins::ProfileMedia::new(
            fixture.services.skins.clone(),
            fixture.services.accounts.clone(),
            auth,
            fixture.services.tasks.clone(),
            fixture
                .services
                .instances
                .directories()
                .library()
                .admit_application_root()
                .unwrap(),
        )
        .unwrap();
        let app = super::super::skin::router(media);
        let (status, visible) = read_route(app.clone(), "/api/v1/skins").await;
        assert_eq!(status, StatusCode::OK, "{visible}");
        assert_eq!(visible["skins"].as_array().unwrap().len(), 2);
        assert!(visible["pending_apply_texture_key"].is_null());
        for (record, png) in &skins {
            let key = record["texture_key"].as_str().unwrap();
            let stored = fixture.services.skins.get(key).unwrap().unwrap();
            assert_eq!(serde_json::to_value(&stored.record).unwrap(), *record);
            let selected = visible["skins"]
                .as_array()
                .unwrap()
                .iter()
                .find(|skin| skin["texture_key"] == key)
                .unwrap();
            let mut expected = record.clone();
            expected["applied_at"] = Value::Null;
            assert_eq!(*selected, expected);
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("/api/v1/skins/{key}/file"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
            assert_eq!(
                to_bytes(response.into_body(), 256 * 1024)
                    .await
                    .unwrap()
                    .as_ref(),
                png
            );
        }
        let first = skins[0].0["texture_key"].as_str().unwrap();
        let second = skins[1].0["texture_key"].as_str().unwrap();
        fixture
            .services
            .skins
            .update_metadata(
                first,
                UpdateSavedSkinRequest {
                    name: Some("User edited after import".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        let edited = fixture.services.skins.get(first).unwrap().unwrap();
        assert!(matches!(
            fixture.services.skins.delete_unapplied(second).unwrap(),
            SavedSkinDeleteResult::Deleted(_)
        ));
        let (status, replay) = fixture
            .request(Method::POST, "/api/v1/import/skins", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["already_imported"], true);
        assert_eq!(replay["receipt"], imported["receipt"]);
        assert_eq!(fixture.services.skins.get(first).unwrap(), Some(edited));
        assert!(fixture.services.skins.get(second).unwrap().is_none());
        assert_eq!(fixture.services.settings.current().unwrap(), settings);
        assert_eq!(fixture.services.accounts.snapshot().unwrap(), accounts);
        assert_eq!(
            Inventory::capture(&fixture.source, &BTreeMap::new())
                .unwrap()
                .preview()
                .fingerprint,
            request["fingerprint"].as_str().unwrap()
        );
        assert_eq!(
            fs::read(fixture.baseline.join("skins/index.json")).unwrap(),
            source_index
        );
        for (record, png) in &skins {
            assert_eq!(
                fs::read(fixture.baseline.join(format!(
                    "skins/files/{}.png",
                    record["texture_key"].as_str().unwrap()
                )))
                .unwrap(),
                *png
            );
        }
        fixture.services.previews.forget().unwrap();
        let reopened = Arc::new(
            MetadataStore::open(fixture._root.path().join("replacement/metadata.sqlite")).unwrap(),
        );
        fixture.services.skins = Arc::new(SavedSkinLibrary::new(
            SavedSkinStore::new(reopened),
            fixture
                .services
                .instances
                .directories()
                .library()
                .admit_application_root()
                .unwrap(),
        ));
        let (status, recorded) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(recorded["receipt"], imported["receipt"]);
        assert_eq!(recorded["cutover_available"], false);
        assert_eq!(fixture.services.skins.list().unwrap().len(), 1);
        assert!(fixture.services.skins.get(second).unwrap().is_none());
        assert!(!fixture.baseline.join(".axial-root.lease").exists());
    }

    #[tokio::test]
    async fn saved_skin_import_collision_rolls_back_batch_and_receipt() {
        let fixture = Fixture::new();
        let skins = [
            saved_skin(33, "New entry"),
            saved_skin(44, "Legacy collision"),
        ];
        let request = fixture.admit_source_skins(&skins);
        let existing = fixture
            .services
            .skins
            .save_upload(
                &skins[1].1,
                axial_app::skins::library::SaveSkinOptions {
                    name: "Existing user entry".into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(existing.texture_key, skins[1].0["texture_key"]);
        let before = fixture.services.skins.list().unwrap();
        let (status, error) = fixture
            .request(Method::POST, "/api/v1/import/skins", request.clone())
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert_eq!(fixture.services.skins.list().unwrap(), before);
        assert!(
            fixture
                .services
                .skins
                .get(skins[0].0["texture_key"].as_str().unwrap())
                .unwrap()
                .is_none()
        );
        let (status, recorded) = fixture
            .request(
                Method::GET,
                &format!(
                    "/api/v1/import/skins/{}",
                    request["skin_import_id"].as_str().unwrap()
                ),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert!(recorded["receipt"].is_null());
    }

    #[tokio::test]
    async fn dropped_skin_import_waiter_commits_after_preview_forget() {
        let fixture = Fixture::new();
        let skins = [saved_skin(55, "Retained accepted skin")];
        let request = fixture.admit_source_skins(&skins);
        let status_path = format!(
            "/api/v1/import/skins/{}",
            request["skin_import_id"].as_str().unwrap()
        );
        let library = fixture.services.skins.clone();
        let (entered, held) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let gate = tokio::task::spawn_blocking(move || {
            library.with_skin(&"0".repeat(64), |_| {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            })
        });
        held.await.unwrap();
        let app = fixture.router();
        let waiter = tokio::spawn(async move {
            app.oneshot(
                Request::post("/api/v1/import/skins")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
        });
        let mut tasks_changed = fixture.services.tasks.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.services.tasks.status().running.is_empty() {
                tasks_changed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        fixture.services.previews.forget().unwrap();
        waiter.abort();
        release.send(()).unwrap();
        gate.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.services.tasks.status().is_idle() {
                tasks_changed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let (status, recorded) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(
            recorded["receipt"]["texture_keys"],
            json!([skins[0].0["texture_key"]])
        );
        assert_eq!(recorded["cutover_available"], false);
        assert_eq!(
            fixture
                .services
                .skins
                .read_png(skins[0].0["texture_key"].as_str().unwrap())
                .unwrap(),
            Some(skins[0].1.clone())
        );
        fixture
            .services
            .tasks
            .shutdown(Duration::from_secs(3))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn saved_skin_import_rejects_caller_authority_and_changed_preview() {
        let fixture = Fixture::new();
        assert!(
            !fixture
                .services
                .previews
                .current()
                .unwrap()
                .skin_import_available
        );
        let skins = [saved_skin(66, "Unpublished skin")];
        let request = fixture.admit_source_skins(&skins);
        let mut caller_path = request.clone();
        caller_path["source_path"] = json!("/private/skin.png");
        let (status, error) = fixture
            .request(Method::POST, "/api/v1/import/skins", caller_path)
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!error.to_string().contains("/private"));
        for field in ["skin_import_id", "fingerprint"] {
            let mut wrong = request.clone();
            wrong[field] = json!("0".repeat(64));
            assert_eq!(
                fixture
                    .request(Method::POST, "/api/v1/import/skins", wrong)
                    .await
                    .0,
                StatusCode::CONFLICT
            );
        }
        fs::write(
            fixture.baseline.join(format!(
                "skins/files/{}.png",
                skins[0].0["texture_key"].as_str().unwrap()
            )),
            b"changed predecessor bytes",
        )
        .unwrap();
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/import/skins", request.clone())
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert!(fixture.services.skins.list().unwrap().is_empty());
        let (status, recorded) = fixture
            .request(
                Method::GET,
                &format!(
                    "/api/v1/import/skins/{}",
                    request["skin_import_id"].as_str().unwrap()
                ),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert!(recorded["receipt"].is_null());
    }

    #[tokio::test]
    async fn mixed_metadata_import_maps_identities_and_requires_microsoft_sign_in() {
        let fixture = Fixture::new();
        fixture.select_source_microsoft(true);
        let preview = fixture.services.previews.current().unwrap();
        assert!(preview.metadata_import_available);
        assert_eq!(preview.offline_account_count, 2);
        assert_eq!(preview.microsoft_reauthentication_count, 1);
        assert!(!preview.cutover_available);
        let before_source = fs::read(fixture.baseline.join("accounts.json")).unwrap();
        let request = fixture.metadata_request();
        let status_path = format!(
            "/api/v1/import/metadata/{}",
            request["metadata_import_id"].as_str().unwrap()
        );
        let (status, imported) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        let receipt = &imported["receipt"];
        assert_eq!(receipt["imported_offline_account_count"], 2);
        assert_eq!(receipt["imported_microsoft_account_count"], 1);
        assert_eq!(
            receipt["account_id_mapping"],
            json!({
                "offline-92d74dda76fe332cb669d0daddfb7952":"offline-92d74dda76fe332cb669d0daddfb7952",
                "offline-761ba00914be3766a1c04a87a961857c":"offline-761ba00914be3766a1c04a87a961857c",
                (LEGACY_MICROSOFT_ID):MICROSOFT_ID
            })
        );
        assert_eq!(imported["cutover_available"], false);
        assert!(!imported.to_string().contains(LEGACY_LOGIN_ID));
        let snapshot = fixture.services.accounts.snapshot().unwrap();
        assert_eq!(snapshot.accounts.len(), 3);
        assert_eq!(snapshot.launch_auth_mode, LaunchAuthMode::Online);
        let active = snapshot.active_account().unwrap();
        assert_eq!(active.account_id.as_str(), MICROSOFT_ID);
        assert_eq!(active.kind, AccountKind::Microsoft);
        assert_eq!(active.display_name, "A");
        assert_eq!(
            active.minecraft_profile_id.as_deref(),
            Some(MICROSOFT_PROFILE)
        );
        assert_eq!(active.credential_revision, 0);
        assert_eq!(active.login_id, None);
        assert_eq!(active.minecraft_profile, None);
        assert_eq!(active.created_at, "2026-09-08T00:02:00Z");
        assert_eq!(active.updated_at, "2026-09-08T00:03:00Z");
        let (status, config) = fixture
            .request(Method::GET, "/api/v1/config", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{config}");
        assert_eq!(config["username"], "A");
        assert_eq!(config["launch_auth_mode"], "online");
        let auth = Arc::new(AuthService::new(
            fixture.services.accounts.clone(),
            Arc::new(CredentialStore::with_task_owner(
                uuid::Uuid::new_v4(),
                fixture.services.tasks.clone(),
            )),
            fixture.services.tasks.clone(),
        ));
        let capture = fixture.services.accounts.capture_selected().unwrap();
        assert!(matches!(
            auth.launch_credentials(&capture).await,
            Err(AuthError::SignInRequired)
        ));
        let auth_app = super::super::accounts::router(auth.clone())
            .merge(super::super::auth::router(auth, false));
        let (status, accounts) = read_route(auth_app.clone(), "/api/v1/accounts").await;
        assert_eq!(status, StatusCode::OK, "{accounts}");
        let account = accounts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["account_id"] == MICROSOFT_ID)
            .unwrap();
        assert_eq!(account["active"], true);
        assert_eq!(
            account["online_action"]["state_id"],
            "online_sign_in_required"
        );
        for field in [
            "msa_authenticated",
            "msa_refresh_available",
            "minecraft_profile_ready",
            "minecraft_ownership_verified",
            "online_mode_ready",
        ] {
            assert_eq!(account[field], false, "{field}: {account}");
        }
        let (status, auth_status) = read_route(auth_app, "/api/v1/auth/status").await;
        assert_eq!(status, StatusCode::OK, "{auth_status}");
        assert_eq!(auth_status["provider"], "microsoft");
        assert_eq!(auth_status["mode"], "online");
        assert_eq!(auth_status["verified"], false);
        assert_eq!(auth_status["online_mode_ready"], false);
        assert_eq!(auth_status["minecraft_profile_ready"], false);
        let (status, replay) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request)
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["already_imported"], true);
        assert_eq!(replay["receipt"], *receipt);
        assert_eq!(fixture.services.accounts.snapshot().unwrap(), snapshot);
        fixture.services.previews.forget().unwrap();
        let (status, recorded) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(recorded["receipt"], *receipt);
        let reopened = Arc::new(
            MetadataStore::open(fixture._root.path().join("replacement/metadata.sqlite")).unwrap(),
        );
        let settings =
            Arc::new(SettingsStore::new_with_telemetry_identity(reopened.clone(), true).unwrap());
        let accounts = Arc::new(AccountDirectory::new(reopened.clone()).unwrap());
        let skins = Arc::new(SavedSkinLibrary::new(
            SavedSkinStore::new(reopened.clone()),
            fixture
                .services
                .instances
                .directories()
                .library()
                .admit_application_root()
                .unwrap(),
        ));
        let tasks = TaskOwner::new(4).unwrap();
        let rules = PerformanceRules::with_remote(reopened.clone(), None, None).unwrap();
        let instances = Arc::new(InstanceService::new(
            InstanceDirectories::new(
                Registry::new(reopened),
                fixture.services.instances.directories().library().clone(),
                Exclusions::new(),
            ),
            tasks.clone(),
        ));
        let reopened_app = router(
            Arc::new(ImportPreviews::new()),
            instances,
            settings,
            accounts.clone(),
            skins,
            rules,
            fixture.services.telemetry.clone(),
            tasks,
        )
        .unwrap();
        let (status, reopened_receipt) = read_route(reopened_app, &status_path).await;
        assert_eq!(status, StatusCode::OK, "{reopened_receipt}");
        assert_eq!(reopened_receipt, recorded);
        assert_eq!(accounts.snapshot().unwrap(), snapshot);
        assert_eq!(
            fs::read(fixture.baseline.join("accounts.json")).unwrap(),
            before_source
        );
        assert!(!fixture.baseline.join(".axial-root.lease").exists());
    }

    #[tokio::test]
    async fn microsoft_only_metadata_import_rejects_credential_fields_before_publication() {
        let fixture = Fixture::new();
        fixture.select_source_microsoft(false);
        let path = fixture.baseline.join("accounts.json");
        let original = fs::read(&path).unwrap();
        let mut malformed: Value = serde_json::from_slice(&original).unwrap();
        malformed["accounts"][0]["refresh_token"] = json!("predecessor-refresh-secret");
        fs::write(&path, serde_json::to_vec(&malformed).unwrap()).unwrap();
        fixture
            .services
            .previews
            .admit(Inventory::capture(&fixture.source, &BTreeMap::new()).unwrap())
            .unwrap();
        assert!(
            !fixture
                .services
                .previews
                .current()
                .unwrap()
                .metadata_import_available
        );
        let request = fixture.metadata_request();
        let (status, error) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request.clone())
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert!(!error.to_string().contains("predecessor-refresh-secret"));
        assert!(
            fixture
                .services
                .accounts
                .snapshot()
                .unwrap()
                .accounts
                .is_empty()
        );
        assert_eq!(fixture.services.settings.current().unwrap().revision, 0);
        let (status, recorded) = fixture
            .request(
                Method::GET,
                &format!(
                    "/api/v1/import/metadata/{}",
                    request["metadata_import_id"].as_str().unwrap()
                ),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert!(recorded["receipt"].is_null());
        fs::write(path, original).unwrap();
        fixture
            .services
            .previews
            .admit(Inventory::capture(&fixture.source, &BTreeMap::new()).unwrap())
            .unwrap();
        let (status, imported) = fixture
            .request(
                Method::POST,
                "/api/v1/import/metadata",
                fixture.metadata_request(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        assert_eq!(imported["receipt"]["imported_offline_account_count"], 0);
        assert_eq!(imported["receipt"]["imported_microsoft_account_count"], 1);
        assert_eq!(
            imported["receipt"]["account_id_mapping"],
            json!({(LEGACY_MICROSOFT_ID):MICROSOFT_ID})
        );
        assert_eq!(imported["cutover_available"], false);
        let snapshot = fixture.services.accounts.snapshot().unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.active_account().unwrap().credential_revision, 0);
        assert_eq!(snapshot.launch_auth_mode, LaunchAuthMode::Online);
    }

    #[tokio::test]
    async fn metadata_import_is_atomic_repeatable_and_has_a_read_only_receipt() {
        let fixture = Fixture::new();
        let request = fixture.metadata_request();
        let status_path = format!(
            "/api/v1/import/metadata/{}",
            request["metadata_import_id"].as_str().unwrap()
        );
        let before_source = fs::read(fixture.baseline.join("accounts.json")).unwrap();
        let (status, response) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response, json!({"receipt":null,"cutover_available":false}));
        let (status, response) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["already_imported"], false);
        assert_eq!(response["cutover_available"], false);
        assert_eq!(response["receipt"]["imported_offline_account_count"], 2);
        assert_eq!(
            response["receipt"]["metadata_import_id"],
            request["metadata_import_id"]
        );
        let accounts = fixture.services.accounts.snapshot().unwrap();
        let settings = fixture.services.settings.current().unwrap();
        assert_eq!(accounts.accounts.len(), 2);
        assert_eq!(
            accounts.active_account().unwrap().display_name,
            "FixturePlayer"
        );
        assert_eq!(settings.username, "FixturePlayer");
        assert!(settings.revision > 0);
        assert_eq!(response["receipt"]["settings_revision"], settings.revision);
        assert_eq!(
            response["receipt"]["account_selection_revision"],
            accounts.selection_revision
        );
        let (status, repeated) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{repeated}");
        assert_eq!(repeated["already_imported"], true);
        assert_eq!(repeated["receipt"], response["receipt"]);
        assert_eq!(fixture.services.accounts.snapshot().unwrap(), accounts);
        assert_eq!(fixture.services.settings.current().unwrap(), settings);
        let (status, saved) = fixture
            .request(
                Method::PUT,
                "/api/v1/config",
                json!({
                    "expected_revision":settings.revision, "telemetry_enabled":true
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        let current = fixture.services.settings.current().unwrap();
        let identity = fixture.services.settings.telemetry_identity().unwrap();
        assert!(identity.is_some());
        let (status, replay) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request)
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["receipt"], response["receipt"]);
        assert_eq!(fixture.services.settings.current().unwrap(), current);
        assert_eq!(
            fixture.services.settings.telemetry_identity().unwrap(),
            identity
        );
        assert!(fixture.services.telemetry.emit(TelemetryEvent::AppStarted {
            state_inspector: false
        }));
        fixture.services.previews.forget().unwrap();
        let (status, recorded) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(
            recorded,
            json!({"receipt":response["receipt"],"cutover_available":false})
        );
        assert_eq!(
            fs::read(fixture.baseline.join("accounts.json")).unwrap(),
            before_source
        );
        assert!(!fixture.baseline.join(".axial-root.lease").exists());
    }

    #[tokio::test]
    async fn metadata_import_rejects_unadmitted_fields_and_stale_source_or_destination() {
        let fixture = Fixture::new();
        let request = fixture.metadata_request();
        for field in ["source_path", "credentials", "settings"] {
            let mut invalid = request.clone();
            invalid[field] = json!("/private/predecessor-secret");
            let (status, error) = fixture
                .request(Method::POST, "/api/v1/import/metadata", invalid)
                .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(!error.to_string().contains("predecessor-secret"));
        }
        let mut stale = request.clone();
        stale["fingerprint"] = json!("0".repeat(64));
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/import/metadata", stale)
                .await
                .0,
            StatusCode::CONFLICT
        );
        fixture
            .services
            .settings
            .update(axial_app::settings::ConfigPatch {
                expected_revision: 0,
                theme: Some(axial_app::settings::ConfigTheme::Birch),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/import/metadata", request)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let stale = fixture.metadata_request();
        fixture
            .services
            .accounts
            .create_offline_account("ExistingPlayer")
            .unwrap();
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/import/metadata", stale)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let current = fixture.metadata_request();
        let settings = fixture.services.settings.current().unwrap();
        let accounts = fixture.services.accounts.snapshot().unwrap();
        fs::write(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt")),
            b"source changed",
        )
        .unwrap();
        assert_eq!(
            fixture
                .request(Method::POST, "/api/v1/import/metadata", current.clone())
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(fixture.services.settings.current().unwrap(), settings);
        assert_eq!(fixture.services.accounts.snapshot().unwrap(), accounts);
        let (_, status) = fixture
            .request(
                Method::GET,
                &format!(
                    "/api/v1/import/metadata/{}",
                    current["metadata_import_id"].as_str().unwrap()
                ),
                Value::Null,
            )
            .await;
        assert!(status["receipt"].is_null());
    }

    #[tokio::test]
    async fn dropped_metadata_waiter_retains_commit_consent_and_read_only_reconciliation() {
        let fixture = Fixture::new();
        fixture.enable_source_telemetry();
        let request = fixture.metadata_request();
        let status_path = format!(
            "/api/v1/import/metadata/{}",
            request["metadata_import_id"].as_str().unwrap()
        );
        let fence = fixture.services.telemetry.consent_change_owned().await;
        let app = fixture.router();
        let waiter = tokio::spawn(async move {
            app.oneshot(
                Request::post("/api/v1/import/metadata")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
        });
        let mut tasks_changed = fixture.services.tasks.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.services.tasks.status().running.is_empty() {
                tasks_changed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        fixture.services.previews.forget().unwrap();
        waiter.abort();
        assert_eq!(fixture.services.settings.current().unwrap().revision, 0);
        drop(fence);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.services.tasks.status().is_idle() {
                tasks_changed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(
            fixture
                .services
                .settings
                .current()
                .unwrap()
                .telemetry_enabled
        );
        assert!(fixture.services.telemetry.emit(TelemetryEvent::AppStarted {
            state_inspector: false
        }));
        fixture.services.previews.forget().unwrap();
        let (status, recorded) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        assert_eq!(recorded["receipt"]["imported_offline_account_count"], 2);
        assert_eq!(recorded["cutover_available"], false);
        assert!(!recorded.to_string().contains("identity"));
        fixture
            .services
            .tasks
            .shutdown(Duration::from_secs(3))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_metadata_commit_rolls_back_accounts_and_does_not_publish_consent_or_receipt() {
        let fixture = Fixture::new();
        fixture.enable_source_telemetry();
        let request = fixture.metadata_request();
        let status_path = format!(
            "/api/v1/import/metadata/{}",
            request["metadata_import_id"].as_str().unwrap()
        );
        fixture.services.settings.metadata().transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("CREATE TRIGGER reject_import_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'/private/predecessor-secret'); END;")?;
            Ok(())
        }).unwrap();
        let (status, error) = fixture
            .request(Method::POST, "/api/v1/import/metadata", request)
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
        assert!(!error.to_string().contains("predecessor-secret"));
        assert!(
            fixture
                .services
                .accounts
                .snapshot()
                .unwrap()
                .accounts
                .is_empty()
        );
        assert_eq!(fixture.services.settings.current().unwrap().revision, 0);
        assert_eq!(
            fixture.services.settings.telemetry_identity().unwrap(),
            None
        );
        assert!(
            !fixture.services.telemetry.emit(TelemetryEvent::AppStarted {
                state_inspector: false
            })
        );
        let (status, response) = fixture
            .request(Method::GET, &status_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert!(response["receipt"].is_null());
    }

    #[tokio::test]
    async fn no_source_has_no_implicit_installed_profile_discovery() {
        let fixture = Fixture::new();
        fixture.services.previews.forget().unwrap();
        let response = preview(State(fixture.services.clone())).await;
        assert_eq!(response.unwrap_err().0, StatusCode::NOT_FOUND);
        assert_eq!(
            fixture
                .post(json!({"fingerprint": "0".repeat(64), "legacy_id": FIRST}))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert!(
            fixture
                .services
                .instances
                .registry()
                .list()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn filesystem_details_do_not_cross_the_public_boundary() {
        let (_, Json(error)) = public_error(ImportError::Io(std::io::Error::other(
            "/private/user/token-secret",
        )));
        assert!(!error.to_string().contains("token-secret"));
        assert!(!error.to_string().contains("/private"));
    }

    #[tokio::test]
    async fn admitted_instance_import_is_repeatable_and_does_not_claim_profile_cutover() {
        let fixture = Fixture::new();
        let streak_path = fixture
            .baseline
            .join("state/persisted-state-rejection-streaks.json");
        fs::create_dir_all(streak_path.parent().unwrap()).unwrap();
        let streak =
            br#"{"schema":"axial.state.persisted_state_rejection_streaks.v1","entries":[]}"#;
        fs::write(&streak_path, streak).unwrap();
        let preview = fixture
            .services
            .previews
            .admit(Inventory::capture(&fixture.source, &BTreeMap::new()).unwrap())
            .unwrap();
        let original = fs::read(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt")),
        )
        .unwrap();
        let request = json!({"fingerprint": preview.fingerprint, "legacy_id": FIRST});
        let (status, response) = fixture.post(request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["legacy_id"], FIRST);
        assert_eq!(response["cutover_available"], false);
        assert_eq!(response["instance"]["name"], "Vanilla Fixture");
        assert_eq!(response["instance"]["extra_jvm_args"], "");
        assert_ne!(response["instance"]["id"], FIRST);
        assert_eq!(
            fixture
                .services
                .instances
                .registry()
                .last_instance_id()
                .unwrap()
                .unwrap()
                .as_str(),
            response["instance"]["id"].as_str().unwrap()
        );
        let (status, repeated) = fixture.post(request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(repeated["instance"]["id"], response["instance"]["id"]);
        assert_eq!(
            fixture.services.instances.registry().list().unwrap().len(),
            1
        );
        assert_eq!(
            original,
            fs::read(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/options.txt"))
            )
            .unwrap()
        );
        assert!(!fixture.baseline.join(".axial-root.lease").exists());
        assert_eq!(fs::read(&streak_path).unwrap(), streak);
        assert!(
            !fixture
                .services
                .previews
                .current()
                .unwrap()
                .cutover_available
        );
    }

    #[tokio::test]
    async fn instance_mapping_route_excludes_pending_and_deleted_publications() {
        let fixture = Fixture::new();
        let preview = fixture.services.previews.current().unwrap();
        let path = format!("/api/v1/import/instances/{}", preview.fingerprint);
        let empty = json!({
            "fingerprint":preview.fingerprint, "metadata_import_id":preview.metadata_import_id,
            "instance_id_mapping":{}, "cutover_available":false
        });
        let before_tasks = fixture.services.tasks.status();
        assert_eq!(
            fixture.request(Method::GET, &path, Value::Null).await,
            (StatusCode::OK, empty.clone())
        );
        assert_eq!(fixture.services.tasks.status(), before_tasks);
        let storage = fixture.services.instances.registry().storage();
        storage.transaction::<_, StorageError>(|transaction| {
            transaction.execute_batch("CREATE TRIGGER reject_mapping_publication BEFORE UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;")?;
            Ok(())
        }).unwrap();
        let request = json!({"fingerprint":preview.fingerprint,"legacy_id":FIRST});
        assert_eq!(
            fixture.post(request.clone()).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fixture.services.instances.pending().unwrap().len(), 1);
        assert_eq!(
            fixture.request(Method::GET, &path, Value::Null).await,
            (StatusCode::OK, empty.clone())
        );
        storage
            .transaction::<_, StorageError>(|transaction| {
                transaction.execute_batch("DROP TRIGGER reject_mapping_publication")?;
                Ok(())
            })
            .unwrap();
        let (status, imported) = fixture.post(request).await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        let id: axial_app::instances::model::InstanceId = imported["instance"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let mut completed = empty.clone();
        completed["instance_id_mapping"] = json!({(FIRST):id});
        assert_eq!(
            fixture.request(Method::GET, &path, Value::Null).await,
            (StatusCode::OK, completed)
        );
        storage
            .migrate(&[
                axial_app::instances::delete::MIGRATION,
                axial_app::content::install::MIGRATION,
                axial_app::performance::mutation::MIGRATION,
                axial_app::performance::mutation::MIGRATION_V2,
            ])
            .unwrap();
        fixture
            .services
            .instances
            .delete(
                &id,
                axial_app::instances::delete::DeleteIntent::KeepFiles,
                uuid::Uuid::new_v4(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(
            fixture
                .services
                .instances
                .registry()
                .list()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fixture.request(Method::GET, &path, Value::Null).await,
            (StatusCode::OK, empty)
        );
        assert_eq!(
            fixture.services.previews.current().unwrap().fingerprint,
            preview.fingerprint
        );
        assert!(!fixture.baseline.join(".axial-root.lease").exists());
    }

    #[tokio::test]
    async fn instance_mapping_route_requires_completion_evidence_for_a_live_instance() {
        let fixture = Fixture::new();
        let preview = fixture.services.previews.current().unwrap();
        let path = format!("/api/v1/import/instances/{}", preview.fingerprint);
        let (status, imported) = fixture
            .post(json!({"fingerprint":preview.fingerprint,"legacy_id":FIRST}))
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        let id = imported["instance"]["id"].as_str().unwrap();
        let (status, completed) = fixture.request(Method::GET, &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{completed}");
        assert_eq!(completed["instance_id_mapping"], json!({(FIRST):id}));
        fixture
            .services
            .instances
            .registry()
            .storage()
            .transaction::<_, StorageError>(|transaction| {
                transaction.execute("DELETE FROM instance_creations WHERE instance_id=?1", [id])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fixture.services.instances.registry().list().unwrap().len(),
            1
        );
        let (status, missing) = fixture.request(Method::GET, &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{missing}");
        assert_eq!(missing["instance_id_mapping"], json!({}));
        assert_eq!(missing["metadata_import_id"], preview.metadata_import_id);
    }

    #[tokio::test]
    async fn instance_mapping_route_is_bound_to_the_current_source_and_exact_fingerprint() {
        let fixture = Fixture::new();
        let original = fixture.services.previews.current().unwrap();
        let path = format!("/api/v1/import/instances/{}", original.fingerprint);
        let (status, imported) = fixture
            .post(json!({"fingerprint":original.fingerprint,"legacy_id":FIRST}))
            .await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        assert_eq!(
            fixture
                .request(
                    Method::GET,
                    &format!("/api/v1/import/instances/{}", "0".repeat(64)),
                    Value::Null
                )
                .await
                .0,
            StatusCode::CONFLICT
        );
        let alternate = fixture._root.path().join("another-predecessor");
        fs::create_dir(&alternate).unwrap();
        copy_fixture(&fixture.baseline, &alternate);
        let source = ReadOnlySource::from_native_selection(
            fixture
                .services
                .instances
                .directories()
                .library()
                .admit_application_root()
                .unwrap(),
            &alternate,
        )
        .unwrap();
        let other = fixture
            .services
            .previews
            .admit(Inventory::capture(&source, &BTreeMap::new()).unwrap())
            .unwrap();
        assert_eq!(other.fingerprint, original.fingerprint);
        assert_ne!(other.metadata_import_id, original.metadata_import_id);
        let (status, unrelated) = fixture.request(Method::GET, &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{unrelated}");
        assert_eq!(
            unrelated,
            json!({
                "fingerprint":original.fingerprint,"metadata_import_id":other.metadata_import_id,
                "instance_id_mapping":{},"cutover_available":false
            })
        );
        fixture
            .services
            .previews
            .admit(Inventory::capture(&fixture.source, &BTreeMap::new()).unwrap())
            .unwrap();
        let (status, completed) = fixture.request(Method::GET, &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{completed}");
        assert_eq!(completed["metadata_import_id"], original.metadata_import_id);
        assert_eq!(
            completed["instance_id_mapping"],
            json!({(FIRST):imported["instance"]["id"]})
        );
        let changed_path = fixture
            .baseline
            .join(format!("instances/{FIRST}/options.txt"));
        fs::write(&changed_path, b"changed source after completion").unwrap();
        assert_eq!(
            fixture.request(Method::GET, &path, Value::Null).await.0,
            StatusCode::CONFLICT
        );
        let changed = fixture
            .services
            .previews
            .admit(Inventory::capture(&fixture.source, &BTreeMap::new()).unwrap())
            .unwrap();
        assert_ne!(changed.fingerprint, original.fingerprint);
        assert_eq!(
            fixture.request(Method::GET, &path, Value::Null).await.0,
            StatusCode::CONFLICT
        );
        let current_path = format!("/api/v1/import/instances/{}", changed.fingerprint);
        let (status, current) = fixture
            .request(Method::GET, &current_path, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{current}");
        assert_eq!(current["instance_id_mapping"], json!({}));
        assert_eq!(current["metadata_import_id"], changed.metadata_import_id);
        fixture.services.previews.forget().unwrap();
        assert_eq!(
            fixture
                .request(Method::GET, &current_path, Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            fs::read(changed_path).unwrap(),
            b"changed source after completion"
        );
        assert!(!fixture.baseline.join(".axial-root.lease").exists());
        assert!(!alternate.join(".axial-root.lease").exists());
    }

    #[tokio::test]
    async fn composed_manual_modded_import_copies_restarts_and_retains_install_target() {
        fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime, u64, u64)> {
            let mut files = BTreeMap::new();
            let mut directories = vec![root.to_owned()];
            while let Some(directory) = directories.pop() {
                for entry in fs::read_dir(directory).unwrap() {
                    let entry = entry.unwrap();
                    let metadata = entry.metadata().unwrap();
                    if metadata.is_dir() {
                        directories.push(entry.path());
                    } else {
                        #[cfg(unix)]
                        let identity = {
                            use std::os::unix::fs::MetadataExt;
                            (metadata.ino(), metadata.nlink())
                        };
                        #[cfg(not(unix))]
                        let identity = (0, 0);
                        files.insert(
                            entry.path().strip_prefix(root).unwrap().to_owned(),
                            (
                                fs::read(entry.path()).unwrap(),
                                metadata.modified().unwrap(),
                                identity.0,
                                identity.1,
                            ),
                        );
                    }
                }
            }
            files
        }
        // Exact predecessor coordinate: Fabric 0.16.9 on Minecraft 1.20.1.
        const VERSION: &str = "loader-v2-YXhpYWwtaW5zdGFsbGVkLWxvYWRlcgABAAYxLjIwLjEABjAuMTYuOQ";
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let baseline = root.path().join("baseline");
        fs::create_dir(&baseline).unwrap();
        copy_fixture(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../acceptance/fixtures/profiles/offline-vanilla"),
            &baseline,
        );
        let registry_path = baseline.join("instances.json");
        let mut registry: Value =
            serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
        registry["instances"][0]["version_id"] = json!(VERSION);
        registry["instances"][0]["loader_key"] = json!("");
        registry["instances"][0]["minecraft_version"] = json!("");
        registry["instances"][0]["performance_mode"] = json!("");
        fs::write(registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
        let config_path = baseline.join("config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["performance_mode"] = json!("custom");
        fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let source_payload = baseline.join("instances").join(FIRST);
        fs::write(source_payload.join("mods/manual.jar"), b"user-managed mod").unwrap();
        fs::write(
            source_payload.join("unknown-user-file.keep"),
            b"unrecognized user data",
        )
        .unwrap();
        let before = snapshot(&baseline);
        let source_files = snapshot(&source_payload);
        let replacement = root.path().join("replacement");
        let services = crate::start_in_profile(replacement.clone(), None)
            .await
            .unwrap();
        let source = ReadOnlySource::from_native_selection(
            services.library.admit_application_root().unwrap(),
            &baseline,
        )
        .unwrap();
        let inventory = Inventory::capture(&source, &BTreeMap::new()).unwrap();
        let preview = services.imports.admit(inventory).unwrap();
        assert!(preview.instances[0].ordinary_import_available);
        assert_eq!(preview.instances[0].loader_key, "fabric");
        assert!(!preview.cutover_available);
        let body = json!({"fingerprint":preview.fingerprint,"legacy_id":FIRST});
        let client = reqwest::Client::new();
        let bootstrap = services.server.bootstrap();
        let imported: Value = client
            .post(format!("{}/api/v1/import/instances", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = imported["instance"]["id"].as_str().unwrap();
        assert_eq!(imported["instance"]["version_id"], VERSION);
        assert_eq!(imported["instance"]["loader_key"], "fabric");
        assert_eq!(imported["instance"]["performance_mode"], "custom");
        assert_eq!(imported["instance"]["auto_optimize"], false);
        assert_eq!(imported["cutover_available"], false);
        let destination = services
            .library
            .admit()
            .unwrap()
            .read_projection()
            .unwrap()
            .join("instances")
            .join(id);
        let copied = snapshot(&destination);
        assert_eq!(
            source_files.keys().collect::<Vec<_>>(),
            copied.keys().collect::<Vec<_>>()
        );
        for (path, original) in &source_files {
            assert_eq!(copied[path].0, original.0);
            #[cfg(unix)]
            {
                assert_ne!(
                    copied[path].2,
                    original.2,
                    "independent inode: {}",
                    path.display()
                );
                assert_eq!(copied[path].3, 1, "no hard links: {}", path.display());
            }
        }
        let detail_path = format!("/api/v1/instances/{id}");
        let detail: Value = client
            .get(format!("{}{detail_path}", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["launchable"], false);
        assert_eq!(detail["needs_install"], VERSION);
        assert_eq!(detail["launch_action"]["primary_action"], "install");
        assert_eq!(detail["install_target"]["version_id"], VERSION);
        assert_eq!(
            detail["install_target"]["loader"]["component_id"],
            "net.fabricmc.fabric-loader"
        );
        assert_eq!(
            detail["install_target"]["loader"]["minecraft_version"],
            "1.20.1"
        );
        assert_eq!(
            detail["install_target"]["loader"]["loader_version"],
            "0.16.9"
        );
        assert_eq!(detail["version_display"]["loader_key"], "fabric");
        assert!(services.sessions.snapshots().is_empty());
        fs::write(
            destination.join("mods/manual.jar"),
            b"later replacement edit",
        )
        .unwrap();
        let edited = snapshot(&destination);
        let repeated: Value = client
            .post(format!("{}/api/v1/import/instances", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(repeated, imported);
        assert_eq!(snapshot(&destination), edited);
        assert_eq!(snapshot(&baseline), before);
        services.imports.forget().unwrap();
        drop(source);
        services.server.shutdown().await.unwrap();
        drop(services);
        let reopened = crate::start_in_profile(replacement, None).await.unwrap();
        let bootstrap = reopened.server.bootstrap();
        let restored: Value = client
            .get(format!("{}{detail_path}", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(restored["id"], id);
        assert_eq!(restored["version_id"], VERSION);
        assert_eq!(restored["performance_mode"], "custom");
        assert_eq!(restored["install_target"], detail["install_target"]);
        assert_eq!(restored["launchable"], false);
        assert_eq!(restored["needs_install"], VERSION);
        assert_eq!(reopened.instances.registry().list().unwrap().len(), 1);
        assert!(reopened.instances.pending().unwrap().is_empty());
        assert!(reopened.sessions.snapshots().is_empty());
        assert_eq!(snapshot(&destination), edited);
        assert_eq!(snapshot(&baseline), before);
        assert!(!baseline.join(".axial-root.lease").exists());
        reopened.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn composed_instance_import_retains_terminal_history_without_a_live_launch() {
        composed_instance_history_import("stopped", None, &["exited"], true, false).await;
    }

    #[tokio::test]
    async fn composed_instance_import_retains_consumed_restart_history_without_a_live_launch() {
        composed_instance_history_import(
            "interrupted",
            Some("driver automatic resume started after restart"),
            &["exited"],
            true,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn composed_instance_import_retains_all_pending_plan_history_without_a_live_launch() {
        composed_instance_history_import("stopped", None, &["pending", "pending"], false, false)
            .await;
    }

    #[tokio::test]
    async fn composed_instance_import_retains_mixed_pending_plan_history_without_a_live_launch() {
        composed_instance_history_import(
            "stopped",
            None,
            &["exited", "pending", "pending"],
            true,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn composed_instance_import_retains_older_driver_with_recreated_pending_plan() {
        composed_instance_history_import("stopped", None, &["pending", "pending"], true, false)
            .await;
    }

    #[tokio::test]
    async fn composed_instance_import_resumes_compatible_pending_plan() {
        composed_instance_history_import("stopped", None, &["pending", "pending"], false, true)
            .await;
    }

    #[tokio::test]
    async fn composed_instance_import_resumes_compatible_mixed_plan() {
        composed_instance_history_import("stopped", None, &["exited", "pending"], true, true).await;
    }

    #[tokio::test]
    async fn composed_instance_import_resumes_compatible_restart_limit_history() {
        composed_instance_history_import(
            "interrupted",
            Some("driver ignored after restart resume limit"),
            &["exited", "pending"],
            true,
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn composed_instance_import_resumes_compatible_queued_handoff_explicitly() {
        for has_report in [false, true] {
            composed_instance_history_import(
                "interrupted",
                Some("driver automatic resume queued after restart"),
                &[if has_report { "exited" } else { "pending" }, "pending"],
                has_report,
                true,
            )
            .await;
        }
    }

    async fn composed_instance_history_import(
        driver_state: &str,
        driver_error: Option<&str>,
        run_states: &[&str],
        has_report: bool,
        canonical_plan: bool,
    ) {
        let can_resume = canonical_plan;
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let baseline = root.path().join("baseline");
        fs::create_dir(&baseline).unwrap();
        copy_fixture(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../acceptance/fixtures/profiles/offline-vanilla"),
            &baseline,
        );
        let history_path = baseline.join("benchmarks/launch/session-a.json");
        fs::create_dir_all(history_path.parent().unwrap()).unwrap();
        let mut legacy_report = json!({
            "schema":"axial.launch.proof", "schema_version":3,
            "session_id":"session-a", "instance_id":FIRST, "version_id":"1.21.1",
            "launched_at":"2026-01-01T00:00:00.000Z", "recorded_at":"2026-01-01T00:00:02.000Z",
            "outcome":"exited", "session_outcome":{"reason":"clean_exit","kind":"clean","summary":"Minecraft exited cleanly."},
            "scenario":{"scenario_id":"vanilla_launch","performance_mode":"vanilla","requested_memory_mb":1024,"version_id":"1.21.1",
                "benchmark_profile":"vanilla_baseline","benchmark_run_type":"coldish","benchmark_mode":"development","benchmark_id":"benchmark-0000000000000001"},
            "device":{"tier":"mid","total_memory_mb":8192,"cpu_threads":8},
            "pid":4321, "exit_code":0, "boot_duration_ms":1000, "stages":[]
        });
        let plan = canonical_plan.then(|| {
            axial_app::performance::benchmarks::benchmark_suite_plan("development").unwrap()
        });
        if let Some(plan) = &plan {
            assert_eq!(run_states.len(), plan.len());
        }
        let runs: Vec<_> = run_states
            .iter()
            .enumerate()
            .map(|(index, state)| {
                let (profile, run_type, target, id) = match &plan {
                    Some(plan) => {
                        let run = plan[index];
                        (run.profile, run.run_type, run.target_id.unwrap_or(""),
                            axial_app::performance::benchmarks::benchmark_suite_run_id("development", index, run))
                    }
                    None => ("vanilla_baseline", "coldish", "", format!("benchmark-{:016x}", index + 1)),
                };
                json!({"run_index":index,"profile":profile,"run_type":run_type,"target_id":target,
                    "benchmark_id":id,
                    "session_id":(*state == "exited").then_some("session-a"),
                    "launched_at":(*state == "exited").then_some("2026-01-01T00:00:00Z"),"state":state})
            })
            .collect();
        legacy_report["scenario"]["benchmark_id"] = runs[0]["benchmark_id"].clone();
        let legacy_report = serde_json::to_vec(&legacy_report).unwrap();
        let mut legacy_suite = json!({
            "schema":"axial.launch.benchmark.suite","schema_version":2,
            "suite_id":"suite-dev-0000000000000001","instance_id":FIRST,"mode":"development",
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z",
            "runs":runs
        });
        if has_report && !run_states.contains(&"exited") {
            legacy_suite["created_at"] = json!("2026-01-02T00:00:00Z");
            legacy_suite["updated_at"] = json!("2026-01-02T00:00:00Z");
        }
        let legacy_driver = json!({
            "id":"benchmark-suite-driver-0000000000000001","suite_id":"suite-dev-0000000000000001",
            "mode":"development","state":driver_state,"interval_ms":30000,
            "run_count":run_states.len(),"launched_run_count":usize::from(has_report),
            "pending_run_index":run_states.iter().position(|state| *state == "pending"),"active_session_id":null,
            "last_run_index":has_report.then_some(0),"last_session_id":has_report.then_some("session-a"),"error":driver_error,
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z"
        });
        let mut source_records = Vec::new();
        if has_report {
            source_records.push((history_path, legacy_report));
        }
        source_records.extend([
            (
                baseline.join("benchmarks/suites/suite-dev-0000000000000001.json"),
                serde_json::to_vec(&legacy_suite).unwrap(),
            ),
            (
                baseline
                    .join("benchmarks/suite-drivers/benchmark-suite-driver-0000000000000001.json"),
                serde_json::to_vec(&legacy_driver).unwrap(),
            ),
        ]);
        for (path, bytes) in &source_records {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let source_canary = baseline.join(format!("instances/{FIRST}/import-preservation.keep"));
        fs::write(&source_canary, b"retain source payload").unwrap();
        let canary_modified = fs::metadata(&source_canary).unwrap().modified().unwrap();
        let replacement = root.path().join("replacement");
        let services = crate::start_in_profile(replacement.clone(), None)
            .await
            .unwrap();
        let source = ReadOnlySource::from_native_selection(
            services.library.admit_application_root().unwrap(),
            &baseline,
        )
        .unwrap();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let request = |method, path: &str| {
            client
                .request(method, format!("{}{path}", bootstrap.base_url))
                .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
        };
        if !canonical_plan && (driver_error.is_some() || run_states.contains(&"pending")) {
            let mut cases = Vec::new();
            if driver_error.is_some() {
                cases.extend([
                    "queued_handoff_wrong_state",
                    "resume_limit_wrong_state",
                    "wrong_state",
                    "active_session",
                    "nonterminal_run",
                    "missing_report",
                    "nonterminal_report",
                    "contradictory_report",
                    "unknown_schema",
                    "unknown_driver_field",
                ]);
            }
            if run_states.contains(&"pending") {
                cases.extend([
                    "pending_session_only",
                    "pending_time_only",
                    "pending_with_launch",
                    "pending_running",
                    "pending_launching",
                    "pending_terminal_without_launch",
                    "pending_active_driver",
                    "pending_nonterminal_driver",
                    "pending_queued_handoff_active",
                    "pending_missing_last_run",
                    "pending_missing_last_report",
                ]);
                if has_report {
                    cases.extend(["pending_last_wrong_descriptor", "pending_last_after_driver"]);
                }
            }
            for case in cases {
                let mut report: Value = if has_report {
                    serde_json::from_slice(&source_records[0].1).unwrap()
                } else {
                    Value::Null
                };
                let mut suite = legacy_suite.clone();
                let mut driver = legacy_driver.clone();
                let pending = run_states
                    .iter()
                    .position(|state| *state == "pending")
                    .unwrap_or(0);
                match case {
                    "queued_handoff_wrong_state" => {
                        driver["state"] = json!("stopped");
                        driver["error"] = json!("driver automatic resume queued after restart");
                    }
                    "resume_limit_wrong_state" => {
                        driver["error"] = json!("driver ignored after restart resume limit");
                        driver["state"] = json!("stopped");
                    }
                    "wrong_state" => driver["state"] = json!("stopped"),
                    "active_session" => driver["active_session_id"] = json!("session-a"),
                    "nonterminal_run" => suite["runs"][0]["state"] = json!("running"),
                    "missing_report" => suite["runs"][0]["session_id"] = json!("missing"),
                    "nonterminal_report" => {
                        report["outcome"] = json!("running");
                        report["session_outcome"] = Value::Null;
                    }
                    "contradictory_report" => report["outcome"] = json!("failed"),
                    "unknown_schema" => suite["schema_version"] = json!(3),
                    "unknown_driver_field" => driver["future"] = json!(true),
                    "pending_session_only" => {
                        suite["runs"][pending]["session_id"] = json!("session-a")
                    }
                    "pending_time_only" => {
                        suite["runs"][pending]["launched_at"] = json!("2026-01-01T00:00:00Z")
                    }
                    "pending_with_launch" => {
                        suite["runs"][pending]["session_id"] = json!("session-a");
                        suite["runs"][pending]["launched_at"] = json!("2026-01-01T00:00:00Z");
                    }
                    "pending_running" => suite["runs"][pending]["state"] = json!("running"),
                    "pending_launching" => suite["runs"][pending]["state"] = json!("launching"),
                    "pending_terminal_without_launch" => {
                        suite["runs"][pending]["state"] = json!("completed")
                    }
                    "pending_active_driver" => driver["active_session_id"] = json!("session-a"),
                    "pending_nonterminal_driver" => driver["state"] = json!("scheduled"),
                    "pending_queued_handoff_active" => {
                        driver["state"] = json!("interrupted");
                        driver["error"] = json!("driver automatic resume queued after restart");
                        driver["active_session_id"] = json!("session-a");
                    }
                    "pending_missing_last_run" => {
                        driver["run_count"] = json!(run_states.len() + 1);
                        driver["last_run_index"] = json!(run_states.len());
                        driver["last_session_id"] = Value::Null;
                    }
                    "pending_missing_last_report" => {
                        driver["last_run_index"] = json!(pending);
                        driver["last_session_id"] = json!("missing");
                    }
                    "pending_last_wrong_descriptor" => {
                        driver["last_run_index"] = json!(run_states.len() - 1);
                        driver["last_session_id"] = json!("session-a");
                    }
                    "pending_last_after_driver" => {
                        driver["last_run_index"] = json!(pending);
                        driver["last_session_id"] = json!("session-a");
                        report["scenario"]["benchmark_id"] =
                            suite["runs"][pending]["benchmark_id"].clone();
                        report["launched_at"] = json!("2026-01-01T00:00:03.000Z");
                        report["recorded_at"] = json!("2026-01-01T00:00:04.000Z");
                    }
                    _ => unreachable!(),
                }
                let mut observations = Vec::new();
                let records = has_report
                    .then_some(report)
                    .into_iter()
                    .chain([suite, driver]);
                for ((path, _), value) in source_records.iter().zip(records) {
                    let bytes = serde_json::to_vec(&value).unwrap();
                    fs::write(path, &bytes).unwrap();
                    observations.push((bytes, fs::metadata(path).unwrap().modified().unwrap()));
                }
                let inventory = Inventory::capture(&source, &BTreeMap::new()).unwrap();
                let fingerprint = inventory.preview().fingerprint;
                services.imports.admit(inventory).unwrap();
                let preview: Value = request(Method::GET, "/api/v1/import/preview")
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(
                    preview["instances"][0]["ordinary_import_available"], false,
                    "{case}"
                );
                let response = request(Method::POST, "/api/v1/import/instances")
                    .json(&json!({"fingerprint":fingerprint,"legacy_id":FIRST}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "{case}"
                );
                assert!(services.instances.registry().list().unwrap().is_empty());
                assert!(services.instances.pending().unwrap().is_empty());
                assert!(services.sessions.snapshots().is_empty());
                for ((path, _), (bytes, modified)) in source_records.iter().zip(observations) {
                    assert_eq!(fs::read(path).unwrap(), bytes, "{case}");
                    assert_eq!(
                        fs::metadata(path).unwrap().modified().unwrap(),
                        modified,
                        "{case}"
                    );
                }
            }
            for (path, bytes) in &source_records {
                fs::write(path, bytes).unwrap();
            }
        }
        let source_modified: Vec<_> = source_records
            .iter()
            .map(|(path, _)| fs::metadata(path).unwrap().modified().unwrap())
            .collect();
        let inventory = Inventory::capture(&source, &BTreeMap::new()).unwrap();
        let source_fingerprint = inventory.preview().fingerprint;
        services.imports.admit(inventory).unwrap();
        let preview: Value = request(Method::GET, "/api/v1/import/preview")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(preview["instances"][0]["ordinary_import_available"], true);
        assert_eq!(preview["cutover_available"], false);
        let mapping_path = format!("/api/v1/import/instances/{source_fingerprint}");
        assert_eq!(
            client
                .get(format!("{}{mapping_path}", bootstrap.base_url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let mappings: Value = request(Method::GET, &mapping_path)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            mappings,
            json!({
                "fingerprint":source_fingerprint,"metadata_import_id":preview["metadata_import_id"],
                "instance_id_mapping":{},"cutover_available":false
            })
        );
        let before: Value = request(Method::GET, "/api/v1/launch/reports")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(before, json!({"reports":[]}));
        let import_request = json!({"fingerprint":source_fingerprint,"legacy_id":FIRST});
        let mut saved: Option<Value> = None;
        let mut saved_benchmarks: Option<(Value, Value)> = None;
        for _ in 0..2 {
            let response = request(Method::POST, "/api/v1/import/instances")
                .json(&import_request)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let imported: Value = response.json().await.unwrap();
            assert_eq!(status, StatusCode::OK, "{imported}");
            assert_eq!(imported["cutover_available"], false);
            assert_ne!(imported["instance"]["id"], FIRST);
            let mappings: Value = request(Method::GET, &mapping_path)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(
                mappings,
                json!({
                    "fingerprint":source_fingerprint,"metadata_import_id":preview["metadata_import_id"],
                    "instance_id_mapping":{(FIRST):imported["instance"]["id"]},"cutover_available":false
                })
            );
            let history: Value = request(Method::GET, "/api/v1/launch/reports")
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            let reports = history["reports"].as_array().unwrap();
            assert_eq!(reports.len(), usize::from(has_report));
            for report in reports {
                assert_eq!(report["schema_version"], 4);
                assert_eq!(report["instance_id"], imported["instance"]["id"]);
                assert_eq!(report["version_id"], "1.21.1");
                assert_eq!(report["recorded_at"], "2026-01-01T00:00:02.000Z");
                assert_eq!(report["session_outcome"]["reason"], "clean_exit");
                assert_eq!(report["boot_duration_ms"], 1000);
                assert_eq!(report["view_model"]["outcome_tone"], "ok");
                assert_eq!(report["logs"], json!([]));
                assert!(report.get("pid").is_none());
                let historical_pid = report["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|stage| stage["evidence"].as_array().unwrap())
                    .find(|evidence| evidence["id"] == "original_pid")
                    .unwrap();
                assert_eq!(historical_pid["details"], json!(["4321"]));
                let id = report["session_id"].as_str().unwrap();
                assert!(id.starts_with("legacy-"));
                assert!(uuid::Uuid::parse_str(id).is_err());
                let detail: Value = request(Method::GET, &format!("/api/v1/launch/reports/{id}"))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(&detail, report);
            }
            let historical_session = reports
                .first()
                .map(|report| report["session_id"].clone())
                .unwrap_or(Value::Null);
            let drivers: Value = request(Method::GET, "/api/v1/launch/benchmark/suite/drivers")
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(drivers["drivers"].as_array().unwrap().len(), 1);
            let driver = &drivers["drivers"][0];
            let driver_id = driver["driver"]["id"].as_str().unwrap();
            let suite_id = driver["driver"]["suite_id"].as_str().unwrap();
            for (id, prefix) in [(driver_id, "legacy-driver-"), (suite_id, "legacy-suite-")] {
                let suffix = id.strip_prefix(prefix).unwrap();
                assert_eq!(suffix.len(), 64);
                assert!(
                    suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                );
            }
            let mut expected_driver = legacy_driver.clone();
            expected_driver["id"] = json!(driver_id);
            expected_driver["suite_id"] = json!(suite_id);
            expected_driver["last_session_id"] = historical_session.clone();
            expected_driver["historical"] = json!(true);
            assert_eq!(driver["driver"], expected_driver);
            assert_eq!(
                driver["view_model"]["state_label"],
                format!("Historical {driver_state} (read-only)")
            );
            assert_eq!(driver["view_model"]["can_stop"], false);
            assert_eq!(driver["view_model"]["can_resume"], can_resume);
            assert!(driver.get("resumed_driver_id").is_none());
            let suite: Value = request(
                Method::GET,
                &format!("/api/v1/launch/benchmark/suites/{suite_id}"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
            let mut expected_suite = legacy_suite.clone();
            expected_suite["suite_id"] = json!(suite_id);
            expected_suite["instance_id"] = imported["instance"]["id"].clone();
            if run_states[0] == "exited" {
                expected_suite["runs"][0]["session_id"] = historical_session;
            }
            expected_suite["historical"] = json!(true);
            assert_eq!(suite, expected_suite);
            assert!(
                suite["runs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|run| run.get("launch_intent").is_none())
            );
            let detail: Value = request(
                Method::GET,
                &format!("/api/v1/launch/benchmark/suite/drivers/{driver_id}"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
            assert_eq!(&detail, driver);
            if let Some((previous_suite, previous_driver)) = &saved_benchmarks {
                assert_eq!(&suite, previous_suite);
                assert_eq!(driver, previous_driver);
            } else {
                saved_benchmarks = Some((suite, driver.clone()));
            }
            if let Some(previous) = &saved {
                assert_eq!(&history, previous);
            } else {
                saved = Some(history);
            }
        }
        let (suite, mut driver) = saved_benchmarks.unwrap();
        let suite_id = suite["suite_id"].as_str().unwrap();
        let driver_id = driver["driver"]["id"].as_str().unwrap().to_owned();
        let suite_path = format!("/api/v1/launch/benchmark/suites/{suite_id}");
        let driver_path = format!("/api/v1/launch/benchmark/suite/drivers/{driver_id}");
        let command = json!({"suite_id":suite_id,"suite_mode":"development","instance_id":suite["instance_id"]});
        for path in [
            "/api/v1/launch/benchmark".to_owned(),
            "/api/v1/launch/benchmark/suite".to_owned(),
            "/api/v1/launch/benchmark/suite/tick".to_owned(),
            "/api/v1/launch/benchmark/suite/driver".to_owned(),
            format!("{driver_path}/stop"),
            format!("{driver_path}/resume"),
        ] {
            if can_resume && path == format!("{driver_path}/resume") {
                continue;
            }
            let response = request(Method::POST, &path)
                .json(&command)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            let error: Value = response.json().await.unwrap();
            assert_eq!(error, json!({"error":"benchmark input is invalid"}));
        }
        let successor = if can_resume {
            assert_eq!(services.benchmarks.resume_interrupted_drivers().unwrap(), 0);
            assert!(services.tasks.status().is_idle());
            assert!(services.sessions.snapshots().is_empty());
            assert_eq!(
                services
                    .instances
                    .registry()
                    .storage()
                    .read(|connection| {
                        connection
                            .query_row("SELECT COUNT(*) FROM launch_intents", [], |row| {
                                row.get::<_, i64>(0)
                            })
                            .map_err(StorageError::from)
                    })
                    .unwrap(),
                0
            );
            let response = request(Method::POST, &format!("{driver_path}/resume"))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let accepted: Value = response.json().await.unwrap();
            assert_eq!(status, StatusCode::OK, "{accepted}");
            let accepted_id = accepted["driver"]["id"].as_str().unwrap().to_owned();
            let accepted_suite_id = accepted["driver"]["suite_id"].as_str().unwrap().to_owned();
            assert_ne!(accepted_id, driver_id);
            assert_ne!(accepted_suite_id, suite_id);
            assert!(accepted["driver"].get("historical").is_none());
            assert!(accepted.get("resumed_driver_id").is_none());
            assert_eq!(accepted["driver"]["run_count"], run_states.len());
            assert_eq!(
                accepted["driver"]["launched_run_count"],
                usize::from(has_report)
            );
            assert_eq!(
                accepted["driver"]["interval_ms"],
                legacy_driver["interval_ms"]
            );

            // This admission fixture has no account or installed runtime. Join
            // its failed attempt before stopping and reopening the successor.
            let mut changed = services.tasks.subscribe();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !services.tasks.status().is_idle() {
                    changed.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            let stopped = request(
                Method::POST,
                &format!("/api/v1/launch/benchmark/suite/drivers/{accepted_id}/stop"),
            )
            .send()
            .await
            .unwrap();
            assert_eq!(stopped.status(), StatusCode::OK);
            let operational: Value = request(
                Method::GET,
                &format!("/api/v1/launch/benchmark/suites/{accepted_suite_id}"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
            assert!(operational.get("historical").is_none());
            assert_eq!(operational["instance_id"], suite["instance_id"]);
            assert_eq!(operational["mode"], suite["mode"]);
            assert_eq!(
                operational["runs"].as_array().unwrap().len(),
                run_states.len()
            );
            for (run, retained) in operational["runs"]
                .as_array()
                .unwrap()
                .iter()
                .zip(suite["runs"].as_array().unwrap())
            {
                if retained["state"] == "pending" {
                    for field in [
                        "run_index",
                        "profile",
                        "run_type",
                        "target_id",
                        "benchmark_id",
                    ] {
                        assert_eq!(run[field], retained[field], "{field}");
                    }
                    assert!(uuid::Uuid::parse_str(run["launch_intent"].as_str().unwrap()).is_ok());
                    assert_ne!(run["session_id"], json!("session-a"));
                } else {
                    assert_eq!(
                        run, retained,
                        "mapped historical evidence must remain exact"
                    );
                    assert!(run.get("launch_intent").is_none());
                }
            }
            driver["view_model"]["can_resume"] = json!(false);
            driver["resumed_driver_id"] = json!(accepted_id);
            let replay: Value = request(Method::POST, &format!("{driver_path}/resume"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(replay["driver"]["id"], accepted_id);
            assert_eq!(replay["driver"]["suite_id"], accepted_suite_id);
            assert_eq!(replay["driver"]["state"], "stopped");
            assert!(services.tasks.status().is_idle());
            let imported: Value = request(Method::POST, "/api/v1/import/instances")
                .json(&import_request)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(imported["instance"]["id"], suite["instance_id"]);
            Some((accepted_id, accepted_suite_id, operational))
        } else {
            None
        };
        for (path, expected) in [(&suite_path, &suite), (&driver_path, &driver)] {
            let actual: Value = request(Method::GET, path)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(&actual, expected);
        }
        assert_eq!(services.instances.registry().list().unwrap().len(), 1);
        assert!(services.instances.pending().unwrap().is_empty());
        let sessions: Value = request(Method::GET, "/api/v1/launch/sessions")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sessions, json!({"sessions":[]}));
        let stored_counts = services
            .instances
            .registry()
            .storage()
            .read::<(i64, i64, i64, i64), StorageError>(|connection| {
                Ok(connection.query_row(
                    "SELECT (SELECT COUNT(*) FROM launch_intents), (SELECT COUNT(*) FROM benchmark_suites),
                     (SELECT COUNT(*) FROM benchmark_drivers), (SELECT COUNT(*) FROM benchmark_drivers WHERE request IS NOT NULL)",
                    [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?)
            })
            .unwrap();
        assert!(stored_counts.0 <= i64::from(can_resume));
        assert_eq!(
            (stored_counts.1, stored_counts.2, stored_counts.3),
            if can_resume { (2, 2, 1) } else { (1, 1, 0) }
        );
        services
            .instances
            .registry()
            .storage()
            .read(|connection| {
                let source_request: Option<Vec<u8>> = connection.query_row(
                    "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                    [&driver_id],
                    |row| row.get(0),
                )?;
                assert!(source_request.is_none());
                if let Some((id, suite_id, _)) = &successor {
                    let (parent, bytes): (String, Vec<u8>) = connection.query_row(
                        "SELECT source_driver_id,request FROM benchmark_drivers WHERE driver_id=?1",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    assert_eq!(parent, driver_id);
                    let captured: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(captured["instance_id"], suite["instance_id"]);
                    assert_eq!(captured["suite_id"], *suite_id);
                    for field in [
                        "username",
                        "max_memory_mb",
                        "min_memory_mb",
                        "client_started_at_ms",
                    ] {
                        assert!(captured[field].is_null(), "{field}");
                    }
                }
                Ok::<_, StorageError>(())
            })
            .unwrap();
        let saved = saved.unwrap();
        for report in saved["reports"].as_array().unwrap() {
            let report_id = report["session_id"].as_str().unwrap();
            for (method, path) in [
                (Method::GET, format!("/api/v1/launch/{report_id}/status")),
                (Method::POST, format!("/api/v1/launch/{report_id}/kill")),
            ] {
                assert_eq!(
                    request(method, &path).send().await.unwrap().status(),
                    StatusCode::BAD_REQUEST
                );
            }
        }
        assert_eq!(
            request(Method::GET, "/api/v1/launch/reports/legacy.invalid")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(services.sessions.snapshots().is_empty());
        assert_eq!(
            Inventory::capture(&source, &BTreeMap::new())
                .unwrap()
                .preview()
                .fingerprint,
            source_fingerprint
        );
        services.imports.forget().unwrap();
        drop(source);
        services.server.shutdown().await.unwrap();
        drop(services);
        let reopened = crate::start_in_profile(replacement, None).await.unwrap();
        let bootstrap = reopened.server.bootstrap();
        let history: Value = client
            .get(format!("{}/api/v1/launch/reports", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(history, saved);
        for (path, expected) in [(&suite_path, &suite), (&driver_path, &driver)] {
            let actual: Value = client
                .get(format!("{}{path}", bootstrap.base_url))
                .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(&actual, expected);
        }
        let response = client
            .post(format!("{}{driver_path}/resume", bootstrap.base_url))
            .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
            .send()
            .await
            .unwrap();
        if let Some((id, suite_id, operational)) = &successor {
            assert_eq!(response.status(), StatusCode::OK);
            let replay: Value = response.json().await.unwrap();
            assert_eq!(replay["driver"]["id"], *id);
            assert_eq!(replay["driver"]["suite_id"], *suite_id);
            assert_eq!(replay["driver"]["state"], "stopped");
            let actual: Value = client
                .get(format!(
                    "{}/api/v1/launch/benchmark/suites/{suite_id}",
                    bootstrap.base_url
                ))
                .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(&actual, operational);
            assert!(reopened.tasks.status().is_idle());
        } else {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({"error":"benchmark input is invalid"})
            );
        }
        assert!(reopened.sessions.snapshots().is_empty());
        assert_eq!(
            reopened
                .instances
                .registry()
                .storage()
                .read::<(i64, i64, i64, i64), StorageError>(|connection| {
                    Ok(connection.query_row(
                        "SELECT (SELECT COUNT(*) FROM launch_intents), (SELECT COUNT(*) FROM benchmark_suites),
                         (SELECT COUNT(*) FROM benchmark_drivers), (SELECT COUNT(*) FROM benchmark_drivers WHERE request IS NOT NULL)",
                        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )?)
                })
                .unwrap(),
            stored_counts
        );
        assert!(
            reopened
                .instances
                .registry()
                .storage()
                .read(|connection| {
                    connection
                        .query_row(
                            "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                            [&driver_id],
                            |row| row.get::<_, Option<Vec<u8>>>(0),
                        )
                        .map_err(StorageError::from)
                })
                .unwrap()
                .is_none()
        );
        reopened.server.shutdown().await.unwrap();
        for ((path, bytes), modified) in source_records.iter().zip(source_modified) {
            assert_eq!(&fs::read(path).unwrap(), bytes);
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
        }
        assert_eq!(fs::read(&source_canary).unwrap(), b"retain source payload");
        assert_eq!(
            fs::metadata(source_canary).unwrap().modified().unwrap(),
            canary_modified
        );
        assert!(!baseline.join(".axial-root.lease").exists());
    }

    #[tokio::test]
    async fn import_endpoint_refuses_caller_paths_stale_preview_and_unknown_instance() {
        let fixture = Fixture::new();
        let fingerprint = fixture.services.previews.current().unwrap().fingerprint;
        let (status, error) = fixture.post(json!({"fingerprint":fingerprint,"legacy_id":FIRST,"source_path":"/private/secret"})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.get("error").is_some());
        assert!(!error.to_string().contains("/private"));
        assert_eq!(
            fixture
                .post(json!({"fingerprint":fingerprint,"legacy_id":"../elsewhere"}))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            fixture
                .post(json!({"fingerprint":"0".repeat(64),"legacy_id":FIRST}))
                .await
                .0,
            StatusCode::CONFLICT
        );
        fs::write(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt")),
            b"source changed",
        )
        .unwrap();
        assert_eq!(
            fixture
                .post(json!({"fingerprint":fingerprint,"legacy_id":FIRST}))
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert!(
            fixture
                .services
                .instances
                .registry()
                .list()
                .unwrap()
                .is_empty()
        );
    }
}
