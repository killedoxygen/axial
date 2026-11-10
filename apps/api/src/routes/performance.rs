//! Authenticated HTTP adaptation for the Performance owner.

use super::error::{ApiError, ApiResult, Extracted};
use axial_app::{
    instances::model::InstanceId,
    performance::{
        PerformanceService,
        health::{
            PerformanceHealthResponse, PerformanceInstallResponse,
            PerformanceInstanceOperationResponse, PerformanceOperationView,
            PerformanceRollbackResponse, disabled_health_response, resolved_health_response,
        },
        model::{
            PerformanceMode, PerformancePlanRequest, PerformancePlanResponse,
            PerformanceRulesStatusResponse, ResolutionRequest,
        },
        plan::{configured_mode, plan_response, resolve_mode, version_target},
    },
    settings::SettingsStore,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
struct PerformanceApi {
    service: Arc<PerformanceService>,
    settings: Arc<SettingsStore>,
}

pub fn router(service: Arc<PerformanceService>, settings: Arc<SettingsStore>) -> Router {
    Router::new()
        .route("/api/v1/performance/status", get(status))
        .route("/api/v1/performance/rules/refresh", post(refresh))
        .route("/api/v1/performance/plan", get(plan))
        .route("/api/v1/performance/health", get(health))
        .route("/api/v1/performance/rollback", get(rollback))
        .route("/api/v1/performance/install", post(install))
        .route(
            "/api/v1/performance/instances/{id}/operation",
            get(instance_operation),
        )
        .route("/api/v1/performance/operations/{id}", get(operation))
        .layer(DefaultBodyLimit::max(8192))
        .with_state(PerformanceApi { service, settings })
}

async fn status(State(api): State<PerformanceApi>) -> Json<PerformanceRulesStatusResponse> {
    Json(api.service.rules().status())
}

async fn refresh(State(api): State<PerformanceApi>) -> ApiResult<PerformanceRulesStatusResponse> {
    Ok(Json(api.service.refresh_rules().await?))
}

async fn plan(
    State(api): State<PerformanceApi>,
    query: Result<Query<PerformancePlanRequest>, QueryRejection>,
) -> ApiResult<PerformancePlanResponse> {
    let input = query.or_refuse(invalid)?;
    let request = resolution(&api, &input)?;
    if let Some(id) = input.instance_id.as_deref() {
        let id = id.parse::<InstanceId>().map_err(|_| invalid())?;
        let resolved = api.service.resolve_and_inspect(&id, request).await?;
        return Ok(Json(plan_response(&resolved.plan)));
    }
    let planned = api.service.rules().plan(request).await?;
    Ok(Json(plan_response(planned.plan())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceQuery {
    instance_id: Option<String>,
}

async fn health(
    State(api): State<PerformanceApi>,
    query: Result<Query<InstanceQuery>, QueryRejection>,
) -> ApiResult<PerformanceHealthResponse> {
    let query = query.or_refuse(invalid)?;
    let id = selected(&api, query.instance_id.as_deref())?;
    let request = resolution(
        &api,
        &PerformancePlanRequest {
            instance_id: Some(id.to_string()),
            ..Default::default()
        },
    )?;
    if request.mode != PerformanceMode::Managed {
        return Ok(Json(disabled_health_response()));
    }
    let resolved = api.service.resolve_and_inspect(&id, request).await?;
    Ok(Json(resolved_health_response(resolved)))
}

async fn rollback(
    State(api): State<PerformanceApi>,
    query: Result<Query<InstanceQuery>, QueryRejection>,
) -> ApiResult<PerformanceRollbackResponse> {
    let query = query.or_refuse(invalid)?;
    let id = selected(&api, query.instance_id.as_deref())?;
    let inspected = api.service.inspect(&id).await?;
    Ok(Json(PerformanceRollbackResponse {
        snapshots: inspected.rollback_snapshots,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    instance_id: Option<String>,
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    action: Option<String>,
    rollback_id: Option<String>,
    queued: Option<bool>,
}

async fn install(
    State(api): State<PerformanceApi>,
    body: Result<Json<InstallRequest>, JsonRejection>,
) -> ApiResult<PerformanceInstallResponse> {
    let body = body.or_refuse(invalid)?;
    let id = selected(&api, body.instance_id.as_deref())?;
    let request = resolution(
        &api,
        &PerformancePlanRequest {
            instance_id: Some(id.to_string()),
            game_version: body.game_version,
            loader: body.loader,
            mode: body.mode,
        },
    )?;
    let action = body.action.as_deref().unwrap_or("apply");
    if body.queued == Some(true) {
        let operation = api.service.submit(&id, request, action, body.rollback_id)?;
        return Ok(Json(PerformanceInstallResponse::queued(operation)));
    }
    let inspected = match action {
        "apply" => api.service.apply(&id, request).await,
        "reapply" => api.service.reapply(&id, request).await,
        "remove" => api.service.remove(&id).await,
        "rollback" => api.service.rollback(&id, body.rollback_id).await,
        _ => return Err(invalid()),
    }?;
    Ok(Json(PerformanceInstallResponse::complete(inspected)))
}

async fn operation(
    State(api): State<PerformanceApi>,
    Path(id): Path<String>,
) -> ApiResult<PerformanceOperationView> {
    let operation = api
        .service
        .operation(&id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "performance operation not found"))?;
    Ok(Json(operation.into()))
}

async fn instance_operation(
    State(api): State<PerformanceApi>,
    Path(id): Path<String>,
) -> ApiResult<PerformanceInstanceOperationResponse> {
    let id = id.parse().map_err(|_| invalid())?;
    Ok(Json(PerformanceInstanceOperationResponse {
        operation: api.service.instance_operation(&id)?.map(Into::into),
    }))
}

fn resolution(
    api: &PerformanceApi,
    input: &PerformancePlanRequest,
) -> Result<ResolutionRequest, ApiError> {
    let settings = api.settings.current().map_err(|_| unavailable())?;
    let record = input
        .instance_id
        .as_deref()
        .map(|raw| {
            let id: InstanceId = raw.parse().map_err(|_| invalid())?;
            api.service
                .instances()
                .registry()
                .get_live(&id)
                .map_err(|_| unavailable())
        })
        .transpose()?;
    let mode = resolve_mode(
        configured_mode(settings.performance_mode),
        record
            .as_ref()
            .map(|r| r.instance.settings.performance_mode.as_str()),
        input.mode.as_deref(),
    )
    .map_err(|_| invalid())?;
    let (game, loader) = version_target(
        record
            .as_ref()
            .map(|r| r.instance.minecraft_version.as_str()),
        record.as_ref().map(|r| r.instance.loader_key.as_str()),
        input.game_version.as_deref(),
        input.loader.as_deref(),
    )
    .map_err(|_| invalid())?;
    Ok(api.service.resolution_request(game, loader, mode.mode))
}

fn selected(api: &PerformanceApi, raw: Option<&str>) -> Result<InstanceId, ApiError> {
    if let Some(raw) = raw {
        return raw.parse().map_err(|_| invalid());
    }
    api.service
        .instances()
        .registry()
        .last_instance_id()
        .map_err(|_| unavailable())?
        .ok_or_else(invalid)
}

fn invalid() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid performance request")
}

fn unavailable() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "performance target is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Profile;
    use axial_app::{
        instances::model::InstancePatch, performance::rules::PerformanceRules,
        storage::StorageError,
    };
    use axial_performance::{
        CompositionPlan, CompositionTier, ManagedArtifactPin, ManagedArtifactRole,
        ManagedArtifactTransferResolver, ManagedCompositionInstallPlan, PerformanceMode,
        RulesCacheSnapshot, RulesSignatureMetadata,
        types::{ManagedMod, ModCondition, VersionFamily},
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha512};
    use std::{
        collections::BTreeMap,
        io::Write,
        path::{Path as FilePath, PathBuf},
        time::Duration,
    };
    use tower::ServiceExt;

    struct ProjectionFixture {
        profile: Profile,
        instance: InstanceId,
        mods: PathBuf,
    }

    impl ProjectionFixture {
        async fn new() -> Self {
            let profile = Profile::start().await;
            let (instance, game) = profile.create_instance().await;
            let instances = &profile.services.instances;
            let revision = instances
                .registry()
                .get_live(&instance)
                .unwrap()
                .instance
                .revision;
            instances
                .update(
                    &instance,
                    InstancePatch {
                        expected_revision: Some(revision),
                        performance_mode: Some("managed".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let mods = game.join("mods");
            std::fs::write(mods.join("iris-mc1.20.1-1.7.0.jar"), jar("iris")).unwrap();
            Self {
                profile,
                instance,
                mods,
            }
        }

        fn router(&self) -> Router {
            router(
                Arc::new(self.profile.services.performance.clone()),
                self.profile.services.settings.clone(),
            )
        }

        fn signed_rules_router(&self) -> Router {
            let key = SigningKey::from_bytes(&[23; 32]);
            let mut manifest = axial_performance::builtin_manifest().unwrap();
            manifest.generated_at = "2026-10-02T00:00:00Z".into();
            for composition in &mut manifest.compositions {
                for artifact in &mut composition.mods {
                    if artifact.slug == "nvidium" {
                        artifact.condition = ModCondition::Always;
                        artifact.hardware_req = None;
                    }
                }
            }
            let signature =
                key.sign(&axial_performance::canonical_manifest_payload(&manifest).unwrap());
            let snapshot = RulesCacheSnapshot {
                rule_source: axial_performance::RuleSource::Remote,
                rule_channel: axial_performance::RuleChannel::Remote,
                schema_version: manifest.schema_version,
                generated_at: manifest.generated_at.clone(),
                updated_at: manifest.generated_at.clone(),
                validation: axial_performance::RulesValidation::Valid,
                manifest,
                signature: RulesSignatureMetadata {
                    signature: hex::encode(signature.to_bytes()),
                    key_id: Some("projection-fixture".into()),
                },
            };
            self.profile
                .services
                .settings
                .metadata()
                .transaction(|db| -> Result<_, StorageError> {
                    db.execute(
                        "INSERT INTO performance_rules(singleton,snapshot) VALUES(1,?1)",
                        [snapshot.encode().unwrap()],
                    )?;
                    Ok(())
                })
                .unwrap();
            let rules = PerformanceRules::with_remote(
                self.profile.services.settings.metadata().clone(),
                Some("https://example.invalid/rules".into()),
                Some(hex::encode(key.verifying_key().to_bytes())),
            )
            .unwrap();
            let service = self
                .profile
                .services
                .performance
                .clone()
                .with_rules_for_test(rules);
            router(Arc::new(service), self.profile.services.settings.clone())
        }

        async fn seed_managed(&self) {
            use axial_minecraft::download::{
                RetryPolicy, TransferClient, TransferClientConfig, TransferOrigin,
            };
            let bytes = jar("fixture_managed");
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let body = bytes.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().route(
                        "/managed.jar",
                        get(move || {
                            let body = body.clone();
                            async move { body }
                        }),
                    ),
                )
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
            let url = format!("http://{address}/managed.jar");
            let transfers = ManagedArtifactTransferResolver::new(
                move |url| async move {
                    if url.host_str() != Some("127.0.0.1") || url.port() != Some(address.port()) {
                        return Err(std::io::Error::other("unexpected fixture origin"));
                    }
                    let origin = TransferOrigin::from_loopback_http_for_test_support(&url)
                        .map_err(std::io::Error::other)?;
                    let config = TransferClientConfig::bounded(
                        Duration::from_secs(2),
                        Duration::from_secs(5),
                        Duration::from_secs(10),
                        vec![origin],
                    )
                    .map_err(std::io::Error::other)?;
                    TransferClient::build(config).map_err(std::io::Error::other)
                },
                RetryPolicy::none(),
            );
            let plan = ManagedCompositionInstallPlan::seal(
                CompositionPlan {
                    composition_id: "fixture-managed".into(),
                    family: VersionFamily::E,
                    loader: "fabric".into(),
                    mode: PerformanceMode::Managed,
                    tier: CompositionTier::Core,
                    jvm_preset: String::new(),
                    warnings: Vec::new(),
                    fallback_reason: String::new(),
                    mods: vec![ManagedMod {
                        artifact_id: "fixture".into(),
                        project_id: "AANobbMI".into(),
                        slug: "fixture".into(),
                        name: "Fixture".into(),
                        condition: ModCondition::Always,
                        version_range: String::new(),
                        exact_game_versions: Vec::new(),
                        hardware_req: None,
                        mutual_exclusions: Vec::new(),
                    }],
                },
                "1.20.1",
                "fabric",
                vec![
                    ManagedArtifactPin::new(
                        "AANobbMI",
                        "abcdefgh",
                        "fixture-managed.jar",
                        url,
                        bytes.len() as u64,
                        format!("{:x}", Sha512::digest(&bytes)),
                        ManagedArtifactRole::Root,
                    )
                    .unwrap(),
                ],
                Vec::new(),
            )
            .unwrap();
            let inspection = self
                .profile
                .services
                .performance
                .install_plan_for_test(&self.instance, plan, transfers)
                .await
                .unwrap();
            assert_eq!(inspection.health, axial_performance::BundleHealth::Healthy);
            assert_eq!(inspection.state.unwrap().installed_mods.len(), 1);
            assert!(!inspection.rollback_snapshots.is_empty());
            stop.send(()).unwrap();
            server.await.unwrap();
        }

        async fn close(self) {
            self.profile.close().await;
        }
    }

    fn jar(id: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("fabric.mod.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(
                serde_json::to_string(&json!({"schemaVersion":1,"id":id,"version":"1.0.0"}))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    async fn get_json(router: Router, path: &str) -> (StatusCode, Value) {
        let response = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn files(root: &FilePath) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut pending = vec![root.to_owned()];
        let mut files = BTreeMap::new();
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    files.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
        files
    }

    #[test]
    fn performance_health_reserves_the_instance_until_its_inspection_finishes() {
        use futures_util::FutureExt;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cleanup = runtime.block_on(async {
            let fixture = ProjectionFixture::new().await;
            let body = std::panic::AssertUnwindSafe(async {
                let instance = fixture
                    .profile
                    .services
                    .instances
                    .registry()
                    .get_live(&fixture.instance)
                    .unwrap()
                    .instance;
                let health_path = format!(
                    "/api/v1/performance/health?instance_id={}",
                    fixture.instance
                );
                let mut health = Box::pin(get_json(fixture.router(), &health_path));
                let waiting = futures_util::poll!(health.as_mut()).is_pending();
                let refused = fixture.profile.services.instances.update_with_sessions(
                    &fixture.instance,
                    InstancePatch {
                        expected_revision: Some(instance.revision),
                        max_memory_mb: Some(2048),
                        ..Default::default()
                    },
                    &fixture.profile.services.sessions,
                );
                let result = health.await;
                let saved = fixture.profile.services.instances.update_with_sessions(
                    &fixture.instance,
                    InstancePatch {
                        expected_revision: Some(instance.revision),
                        max_memory_mb: Some(2048),
                        ..Default::default()
                    },
                    &fixture.profile.services.sessions,
                );
                move || {
                    assert!(waiting);
                    assert!(matches!(
                        refused,
                        Err(axial_app::instances::model::InstanceError::Busy)
                    ));
                    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
                    assert_eq!(saved.unwrap().settings.max_memory_mb, 2048);
                }
            })
            .catch_unwind()
            .await;
            let shutdown = std::panic::AssertUnwindSafe(tokio::time::timeout(
                Duration::from_secs(30),
                fixture.profile.services.server.shutdown(),
            ))
            .catch_unwind()
            .await;
            let settled = fixture.profile.services.server.is_shutdown_settled()
                && fixture.profile.services.tasks.shutdown_receipt().is_some()
                && fixture.profile.services.tasks.status().is_idle();
            if !matches!(shutdown, Ok(Ok(Ok(())))) || !settled {
                return Err(fixture);
            }
            drop(fixture.profile.services);
            let verified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                body.unwrap_or_else(|panic| std::panic::resume_unwind(panic))();
            }));
            if let Err(panic) = verified {
                eprintln!(
                    "Retained health fixture: {}",
                    fixture.profile.temporary.keep().display()
                );
                std::panic::resume_unwind(panic);
            }
            Ok(())
        });
        if let Err(fixture) = cleanup {
            let retained = fixture.profile.temporary.path().to_owned();
            std::mem::forget((runtime, fixture));
            panic!(
                "Health cleanup did not prove settlement; retained owners and fixture: {}",
                retained.display()
            );
        }
    }

    #[tokio::test]
    async fn performance_projection_instance_plan_uses_admitted_installed_mods() {
        let fixture = ProjectionFixture::new().await;
        let router = fixture.signed_rules_router();
        let before = files(&fixture.mods);
        let query = "/api/v1/performance/plan?game_version=1.20.1&loader=fabric&mode=managed";
        let request_only = get_json(router.clone(), query).await;
        let instance = get_json(
            router.clone(),
            &format!("{query}&instance_id={}", fixture.instance),
        )
        .await;
        let after = files(&fixture.mods);
        drop(router);
        fixture.close().await;
        assert_eq!(request_only.0, StatusCode::OK, "{}", request_only.1);
        assert!(
            request_only.1["effective"]["managed_artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|artifact| artifact["slug"] == "nvidium")
        );
        assert_eq!(instance.0, StatusCode::OK, "{}", instance.1);
        assert!(
            instance.1["effective"]["managed_artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|artifact| artifact["slug"] != "nvidium"),
            "{}",
            instance.1
        );
        assert!(
            instance.1["effective"]["explanation"]["details"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "nvidium skipped: incompatible with managed mod iris")
        );
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn performance_projection_managed_health_retains_plan_warnings() {
        let fixture = ProjectionFixture::new().await;
        let router = fixture.signed_rules_router();
        let before = files(&fixture.mods);
        let response = get_json(
            router.clone(),
            &format!(
                "/api/v1/performance/health?instance_id={}",
                fixture.instance
            ),
        )
        .await;
        let after = files(&fixture.mods);
        drop(router);
        fixture.close().await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        assert!(
            response.1["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "nvidium skipped: incompatible with managed mod iris"),
            "{}",
            response.1
        );
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn performance_health_plan_mismatch_does_not_claim_file_damage() {
        let fixture = ProjectionFixture::new().await;
        fixture.seed_managed().await;
        let before = files(&fixture.mods);
        let response = get_json(
            fixture.router(),
            &format!(
                "/api/v1/performance/health?instance_id={}",
                fixture.instance
            ),
        )
        .await;
        let after = files(&fixture.mods);
        fixture.close().await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        assert_eq!(response.1["health"], "invalid");
        assert_eq!(
            response.1["state"]["installed_mods"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            response.1["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| {
                    warning == "managed composition does not match the current declarative plan"
                })
        );
        assert_eq!(before, after);
        assert_eq!(
            response.1["view_model"]["detail"],
            "The managed bundle could not be validated."
        );
    }

    #[tokio::test]
    async fn performance_projection_nonmanaged_health_is_disabled_without_file_changes() {
        let fixture = ProjectionFixture::new().await;
        fixture.seed_managed().await;
        let before = files(&fixture.mods);
        let mut responses = Vec::new();
        for mode in ["custom", "vanilla"] {
            fixture
                .profile
                .services
                .instances
                .update(
                    &fixture.instance,
                    serde_json::from_value::<InstancePatch>(json!({"performance_mode":mode}))
                        .unwrap(),
                )
                .unwrap();
            let admission = fixture
                .profile
                .services
                .performance
                .instances()
                .admit(&fixture.instance)
                .unwrap();
            responses.push(
                get_json(
                    fixture.router(),
                    &format!(
                        "/api/v1/performance/health?instance_id={}",
                        fixture.instance
                    ),
                )
                .await,
            );
            admission.validate_current().unwrap();
            drop(admission);
        }
        let after = files(&fixture.mods);
        let pending = fixture
            .profile
            .services
            .settings
            .metadata()
            .read(|db| -> Result<i64, StorageError> {
                Ok(
                    db.query_row("SELECT count(*) FROM performance_operations", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        fixture.close().await;
        for (status, response) in responses {
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["health"], "disabled", "{response}");
            assert_eq!(response["warnings"], json!([]));
            assert_eq!(response["view_model"]["tone"], "mute");
            assert_eq!(response["view_model"]["actions"], json!([]));
            assert_eq!(response["view_model"]["title"], "No managed bundle");
            assert_eq!(response["rollback_available"], false);
        }
        assert_eq!(before, after);
        assert_eq!(pending, 0);
    }
}
