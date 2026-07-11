use super::*;
use crate::state::performance_operations::{
    PERFORMANCE_COMMITTING_COMPLETE_STATE, PerformanceOperationPayload,
};
use crate::state::{AppStateInit, DownloadProgress, InstallStore, SessionStore};
use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
use axial_performance::modrinth::ModrinthError;
use axial_performance::{CompositionState, InstalledMod, PerformanceManager};
use axum::{
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::Request,
};
use ed25519_dalek::{Signer, SigningKey};
use std::{
    collections::HashSet,
    fs, io,
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc, Mutex as SyncMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
use crate::state::OperationJournalStore;
use crate::state::contracts::TargetDescriptor;
use crate::state::performance_operations::PerformanceOperationStore;

type PlanQuery = PerformancePlanRequest;
type HealthQuery = PerformanceHealthRequest;
type RollbackQuery = PerformanceRollbackListRequest;
type InstallRequest = PerformanceInstallRequest;

fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route(
            "/api/v1/performance/status",
            axum::routing::get(handle_status),
        )
        .route(
            "/api/v1/performance/rules/refresh",
            axum::routing::post(handle_rules_refresh),
        )
        .route("/api/v1/performance/plan", axum::routing::get(handle_plan))
        .route(
            "/api/v1/performance/health",
            axum::routing::get(handle_health),
        )
        .route(
            "/api/v1/performance/rollback",
            axum::routing::get(handle_rollback_list),
        )
        .route(
            "/api/v1/performance/install",
            axum::routing::post(handle_install),
        )
        .route(
            "/api/v1/performance/instances/{instance_id}/operation",
            axum::routing::get(handle_instance_operation),
        )
        .route(
            "/api/v1/performance/operations/{id}",
            axum::routing::get(handle_operation_status),
        )
}

async fn handle_status(
    State(state): State<AppState>,
) -> Result<
    Json<crate::application::PerformanceRulesStatusResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    Ok(Json(crate::application::performance_rules_status(&state)))
}

async fn handle_rules_refresh(
    State(state): State<AppState>,
) -> Result<
    Json<crate::application::PerformanceRulesStatusResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let request = state.try_admit_request().expect("admit refresh request");
    crate::application::refresh_performance_rules(&state, request.producer_handoff())
        .await
        .map(Json)
        .map_err(crate::application::refresh_performance_rules_error_response)
}

async fn handle_plan(
    State(state): State<AppState>,
    Query(query): Query<PlanQuery>,
) -> Result<Json<PerformancePlanResponse>, (StatusCode, Json<serde_json::Value>)> {
    performance_plan(&state, query).await.map(Json)
}

async fn handle_health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<PerformanceHealthResponse>, (StatusCode, Json<serde_json::Value>)> {
    performance_health(&state, query).await.map(Json)
}

async fn handle_rollback_list(
    State(state): State<AppState>,
    Query(query): Query<RollbackQuery>,
) -> Result<Json<PerformanceRollbackListResponse>, (StatusCode, Json<serde_json::Value>)> {
    performance_rollback_list(&state, query).await.map(Json)
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<PerformanceInstallResponse>, (StatusCode, Json<serde_json::Value>)> {
    let request = state.try_admit_request().expect("admit install request");
    performance_install(state, payload, request.producer_handoff())
        .await
        .map(Json)
}

async fn handle_operation_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Json<crate::application::PerformanceOperationStatusResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    performance_operation_status(&state, &id).await.map(Json)
}

async fn handle_instance_operation(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
) -> Result<Json<PerformanceInstanceOperationResponse>, (StatusCode, Json<serde_json::Value>)> {
    performance_instance_operation(&state, &instance_id)
        .await
        .map(Json)
}

mod install_rollback;
mod operations;
mod plan_health;
mod rules_status;

async fn collect_install_events(state: &AppState, install_id: &str) -> Vec<DownloadProgress> {
    let (mut events, mut receiver, done) = state
        .installs()
        .subscribe(install_id)
        .await
        .expect("install session should exist");
    if done || events.iter().any(|event| event.done) {
        return events;
    }

    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("progress event should arrive")
            .expect("progress receiver should stay open");
        let terminal = event.done;
        events.push(event);
        if terminal {
            return events;
        }
    }
}

fn json_error_message(error: &(StatusCode, Json<serde_json::Value>)) -> String {
    error
        .1
        .0
        .get("error")
        .and_then(|value| value.as_str())
        .expect("json error message")
        .to_string()
}

fn assert_omits_raw_fragments(body: &str, fragments: &[&str]) {
    for fragment in fragments {
        assert!(
            !body.contains(fragment),
            "bounded error body should not contain {fragment:?}: {body}"
        );
    }
}

struct TestFixture {
    state: AppState,
    root: PathBuf,
}

#[derive(Default)]
struct ScriptedOperationBackend {
    attempts: AtomicUsize,
    fail_all: AtomicBool,
    fail_attempts: SyncMutex<HashSet<usize>>,
    gate_attempt: AtomicUsize,
    release_gate: AtomicBool,
}

impl ScriptedOperationBackend {
    fn coordinator(self: &Arc<Self>) -> PersistenceCoordinator {
        PersistenceCoordinator::for_test(
            self.clone(),
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
    }

    fn fail_attempt(&self, attempt: usize) {
        self.fail_attempts
            .lock()
            .expect("scripted failures lock")
            .insert(attempt);
    }

    fn set_fail_all(&self, fail: bool) {
        self.fail_all.store(fail, Ordering::SeqCst);
    }

    fn gate_attempt(&self, attempt: usize) {
        self.gate_attempt.store(attempt, Ordering::SeqCst);
        self.release_gate.store(false, Ordering::SeqCst);
    }

    fn release(&self) {
        self.release_gate.store(true, Ordering::SeqCst);
    }

    async fn wait_for_attempt(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.attempts.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted persistence attempt");
    }
}

impl AtomicWriteBackend for ScriptedOperationBackend {
    fn write(
        &self,
        _target: &TargetDescriptor,
        destination: &FsPath,
        contents: &[u8],
    ) -> io::Result<()> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.gate_attempt.load(Ordering::SeqCst) == attempt {
            while !self.release_gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if self.fail_all.load(Ordering::SeqCst)
            || self
                .fail_attempts
                .lock()
                .expect("scripted failures lock")
                .remove(&attempt)
        {
            return Err(io::Error::other("injected operation persistence failure"));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(destination, contents)
    }
}

impl TestFixture {
    fn new(name: &str) -> Self {
        Self::new_with_remote_url(name, None)
    }

    fn new_with_remote_url(name: &str, remote_rules_url: Option<String>) -> Self {
        Self::new_with_remote_url_and_public_key(name, remote_rules_url, None)
    }

    fn new_with_remote_url_and_public_key(
        name: &str,
        remote_rules_url: Option<String>,
        remote_rules_public_key: Option<String>,
    ) -> Self {
        let root = test_root(name);
        let state = build_test_state(&root, remote_rules_url, remote_rules_public_key);

        Self { state, root }
    }

    fn add_instance(&self, name: &str, version_id: &str) -> String {
        self.state
            .instances()
            .add(
                name.to_string(),
                version_id.to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance")
            .id
    }

    fn write_fabric_version(&self, version_id: &str, minecraft_version: &str) {
        let version_dir = self.root.join("library").join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("create version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": version_id,
                "inheritsFrom": minecraft_version,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }))
            .expect("serialize version"),
        )
        .expect("write version json");
        fs::write(
            version_dir.join(".axial-loader.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "component_id": "net.fabricmc.fabric-loader",
                "component_name": "Fabric",
                "build_id": format!("fabric:{minecraft_version}:0.16.10"),
                "minecraft_version": minecraft_version,
                "loader_version": "0.16.10",
                "build_meta": {}
            }))
            .expect("serialize loader metadata"),
        )
        .expect("write loader metadata");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write version jar");
    }
}

async fn spawn_rules_server(body: Vec<u8>, signature: Option<String>) -> String {
    spawn_delayed_rules_server(body, signature, Duration::ZERO).await
}

async fn spawn_delayed_rules_server(
    body: Vec<u8>,
    signature: Option<String>,
    delay: Duration,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rules server");
    let addr = listener.local_addr().expect("rules server addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept rules request");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        tokio::time::sleep(delay).await;
        let signature_header = signature
            .as_ref()
            .map(|signature| {
                format!(
                    "{}: {}\r\n{}: test-key\r\n",
                    axial_performance::RULES_SIGNATURE_HEADER,
                    signature,
                    axial_performance::RULES_KEY_ID_HEADER
                )
            })
            .unwrap_or_default();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
            signature_header,
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write rules response header");
        socket
            .write_all(&body)
            .await
            .expect("write rules response body");
    });
    format!("http://{addr}/rules.json")
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn test_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "axial-api-performance-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create test root");
    path
}

fn test_paths(root: &std::path::Path) -> AppPaths {
    let config_dir = root.join("config");
    AppPaths {
        config_file: config_dir.join("config.json"),
        instances_file: config_dir.join("instances.json"),
        instances_dir: root.join("instances"),
        music_dir: root.join("music"),
        library_dir: root.join("library"),
        config_dir,
    }
}

fn build_test_state(
    root: &FsPath,
    remote_rules_url: Option<String>,
    remote_rules_public_key: Option<String>,
) -> AppState {
    let paths = test_paths(root);
    let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
    config
        .replace_in_memory(AppConfig {
            library_dir: paths.library_dir.to_string_lossy().to_string(),
            ..config.current()
        })
        .expect("configure library dir");
    let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
    AppState::new(AppStateInit {
        app_name: "Axial".to_string(),
        version: "test".to_string(),
        config,
        instances,
        installs: Arc::new(InstallStore::new()),
        sessions: Arc::new(SessionStore::new()),
        performance: Arc::new(
            PerformanceManager::new_with_config_dir_remote_url_and_public_key(
                &paths.config_dir,
                remote_rules_url,
                remote_rules_public_key,
            )
            .expect("performance manager"),
        ),
        startup_warnings: Vec::new(),
        frontend_dir: root.join("frontend"),
    })
}

fn build_test_state_with_operation_backends(
    root: &FsPath,
    journal_backend: Arc<ScriptedOperationBackend>,
    status_backend: Arc<ScriptedOperationBackend>,
) -> AppState {
    let state = build_test_state(root, None, None);
    replace_operation_backends(state, root, journal_backend, status_backend)
}

fn replace_operation_backends(
    state: AppState,
    root: &FsPath,
    journal_backend: Arc<ScriptedOperationBackend>,
    status_backend: Arc<ScriptedOperationBackend>,
) -> AppState {
    let paths = test_paths(root);
    let journals = Arc::new(
        OperationJournalStore::try_load_from_paths_with_coordinator(
            &paths,
            journal_backend.coordinator(),
        )
        .expect("scripted journal store"),
    );
    let performance_operations = Arc::new(
        PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            status_backend.coordinator(),
        )
        .expect("scripted performance status store"),
    );
    state.with_operation_stores(journals, performance_operations)
}

fn seed_repeated_performance_memory(state: &AppState, composition_id: &str, count: u32) {
    let target = crate::state::contracts::TargetDescriptor::new(
        crate::state::contracts::StabilizationSystem::Performance,
        crate::state::contracts::TargetKind::PerformanceComposition,
        composition_id,
        crate::state::contracts::OwnershipClass::CompositionManaged,
    );
    let mut entry = crate::state::failure_memory::GuardianFailureMemoryEntry::observed(
        crate::guardian::DiagnosisId::new("performance_fallback_selected"),
        crate::guardian::GuardianDomain::Performance,
        target,
        crate::guardian::GuardianMode::Managed,
        Some("intent"),
        "2026-06-15T12:00:00Z",
    );
    entry.occurrence_count = count;
    state
        .failure_memory()
        .record(entry)
        .expect("record performance failure memory");
}

fn test_operation_payload() -> PerformanceOperationPayload {
    PerformanceOperationPayload {
        game_version: None,
        loader: None,
        mode: None,
        rollback_id: None,
    }
}

fn test_performance_display() -> PerformanceInstanceDisplay {
    PerformanceInstanceDisplay {
        memory: PerformanceMemoryDisplay {
            min_gb: 1.0,
            max_gb: 4.0,
            label: "1 to 4 GB".to_string(),
        },
        runtime: PerformanceRuntimeDisplay {
            detected: true,
            label: "Java 17".to_string(),
        },
        mode: PerformanceModeDisplay {
            mode: "managed".to_string(),
            label: "Managed".to_string(),
            source: "global".to_string(),
            source_label: "Global default".to_string(),
        },
    }
}

struct SignedRulesResponse {
    public_key: String,
    signature: String,
}

fn nvidium_always_manifest(generated_at: &str) -> axial_performance::Manifest {
    let mut manifest = axial_performance::builtin_manifest().expect("builtin manifest");
    manifest.generated_at = generated_at.to_string();
    for composition in &mut manifest.compositions {
        for managed_mod in &mut composition.mods {
            if managed_mod.slug == "nvidium" {
                managed_mod.condition = axial_performance::types::ModCondition::Always;
                managed_mod.hardware_req = None;
            }
        }
    }
    manifest
}

fn signed_rules_response(manifest: &axial_performance::Manifest) -> SignedRulesResponse {
    let signing_key = SigningKey::from_bytes(&[13_u8; 32]);
    let payload = axial_performance::canonical_manifest_payload(manifest).expect("payload");
    let signature = signing_key.sign(&payload);
    SignedRulesResponse {
        public_key: hex::encode(signing_key.verifying_key().to_bytes()),
        signature: hex::encode(signature.to_bytes()),
    }
}

fn test_composition_state(
    composition_id: &str,
    installed_mods: Vec<InstalledMod>,
) -> CompositionState {
    CompositionState {
        composition_id: composition_id.to_string(),
        tier: CompositionTier::Core,
        installed_mods,
        installed_at: "2026-05-30T00:00:00Z".to_string(),
        failure_count: 0,
        last_failure: String::new(),
    }
}

fn test_installed_mod(project_id: &str, filename: &str) -> InstalledMod {
    InstalledMod {
        project_id: project_id.to_string(),
        version_id: "version".to_string(),
        filename: filename.to_string(),
        ownership_class: axial_performance::OwnershipClass::CompositionManaged,
        source: test_modrinth_source(),
        integrity: axial_performance::ManagedArtifactIntegrity {
            sha512: String::new(),
            sha512_verified: false,
        },
    }
}

fn test_modrinth_source() -> axial_performance::ManagedArtifactSource {
    axial_performance::ManagedArtifactSource {
        provider: axial_performance::ManagedArtifactProvider::Modrinth,
    }
}

fn valid_sha512() -> String {
    "a".repeat(128)
}
