//! Authenticated HTTP adaptation for the Performance owner.

use super::error::{ApiError, ApiResult, Extracted};
use axial_app::{
    instances::model::InstanceId,
    performance::{
        PerformanceService,
        health::{
            PerformanceHealthResponse, PerformanceInstanceOperationResponse,
            disabled_health_response, resolved_health_response,
        },
        model::{PerformanceMode, ResolutionRequest},
        plan::{configured_mode, resolve_mode, version_target},
    },
    settings::SettingsStore,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
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
        .route("/api/v1/performance/health", get(health))
        .route(
            "/api/v1/performance/instances/{id}/operation",
            get(instance_operation),
        )
        .layer(DefaultBodyLimit::max(8192))
        .with_state(PerformanceApi { service, settings })
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
    let request = resolution(&api, &id)?;
    if request.mode != PerformanceMode::Managed {
        return Ok(Json(disabled_health_response()));
    }
    let resolved = api.service.resolve_and_inspect(&id, request).await?;
    Ok(Json(resolved_health_response(resolved)))
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

fn resolution(api: &PerformanceApi, id: &InstanceId) -> Result<ResolutionRequest, ApiError> {
    let settings = api.settings.current().map_err(|_| unavailable())?;
    let record = api
        .service
        .instances()
        .registry()
        .get_live(id)
        .map_err(|_| unavailable())?;
    let mode = resolve_mode(
        configured_mode(settings.performance_mode),
        Some(&record.instance.settings.performance_mode),
        None,
    )
    .map_err(|_| invalid())?;
    let (game, loader) = version_target(
        Some(&record.instance.minecraft_version),
        Some(&record.instance.loader_key),
        None,
        None,
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
        instances::model::InstancePatch,
        performance::{
            composition::{
                ManagedArtifactPin, ManagedArtifactTransferResolver, ManagedCompositionInstallPlan,
            },
            model::{
                BundleHealth, CompositionPlan, CompositionTier, ManagedArtifactRole, ManagedMod,
                ModCondition, PerformanceMode, RuleChannel, RuleSource, RulesValidation,
                VersionFamily,
            },
            resolve::builtin_manifest,
            rules::{
                PerformanceRules, RulesCacheSnapshot, RulesSettings, RulesSignatureMetadata,
                canonical_manifest_payload,
            },
        },
        storage::StorageError,
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
            let mut manifest = builtin_manifest().unwrap();
            manifest.generated_at = "2026-10-02T00:00:00Z".into();
            for composition in &mut manifest.compositions {
                for artifact in &mut composition.mods {
                    if artifact.slug == "nvidium" {
                        artifact.condition = ModCondition::Always;
                        artifact.hardware_req = None;
                    }
                }
            }
            let signature = key.sign(&canonical_manifest_payload(&manifest).unwrap());
            let snapshot = RulesCacheSnapshot {
                rule_source: RuleSource::Remote,
                rule_channel: RuleChannel::Remote,
                schema_version: manifest.schema_version,
                generated_at: manifest.generated_at.clone(),
                updated_at: manifest.generated_at.clone(),
                validation: RulesValidation::Valid,
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
            let rules = PerformanceRules::new(
                self.profile.services.settings.metadata().clone(),
                RulesSettings::new(
                    Some("https://example.invalid/rules"),
                    Some(&hex::encode(key.verifying_key().to_bytes())),
                    None,
                ),
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
            assert_eq!(inspection.health, BundleHealth::Healthy);
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

    const STARTUP_RULES: &str = "2001-01-01T00:00:00Z";

    fn signed_rules_response(key: &SigningKey, generated_at: &str) -> String {
        let mut manifest = builtin_manifest().unwrap();
        manifest.generated_at = generated_at.into();
        let body = String::from_utf8(canonical_manifest_payload(&manifest).unwrap()).unwrap();
        let signature = hex::encode(key.sign(body.as_bytes()).to_bytes());
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {signature}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Serves one canned response per request, in order. Each request is
    /// reported once its head is read; a gated response waits for release.
    fn rules_provider(
        responses: Vec<(Option<tokio::sync::oneshot::Receiver<()>>, String)>,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/rules", listener.local_addr().unwrap());
        let (accepted, requests) = tokio::sync::mpsc::unbounded_channel();
        let provider = tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            for (gate, response) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                let mut chunk = [0; 1024];
                while !head.ends_with(b"\r\n\r\n") {
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0 && head.len() + count <= 8192);
                    head.extend_from_slice(&chunk[..count]);
                }
                assert!(head.starts_with(b"GET /rules HTTP/1.1\r\n"));
                accepted.send(()).unwrap();
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (url, requests, provider)
    }

    async fn configured_profile(url: &str, key: &SigningKey) -> Profile {
        Profile::start_with(crate::TestInputs {
            rules: RulesSettings::new(
                Some(url),
                Some(&hex::encode(key.verifying_key().to_bytes())),
                None,
            ),
            ..Default::default()
        })
        .await
    }

    #[tokio::test]
    async fn configured_rules_refresh_at_startup() {
        let key = SigningKey::from_bytes(&[23; 32]);
        let (url, mut requests, provider) =
            rules_provider(vec![(None, signed_rules_response(&key, STARTUP_RULES))]);
        let profile = configured_profile(&url, &key).await;
        let rules = profile.services.performance.rules().clone();
        let tasks = profile.services.tasks.clone();
        requests.recv().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while rules.status().status.generated_at != STARTUP_RULES || !tasks.status().is_idle() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("startup did not publish the configured provider's rules");

        profile.close().await;
        provider.await.unwrap();
        let status = rules.status().status;
        assert_eq!(status.generated_at, STARTUP_RULES);
        assert_eq!(status.rule_source, RuleSource::Remote);
        assert!(status.warnings.is_empty());
    }
}
