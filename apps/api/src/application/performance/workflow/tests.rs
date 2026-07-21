use super::*;
use crate::state::performance_operations::{
    PERFORMANCE_COMMITTING_COMPLETE_STATE, PerformanceOperationPayload,
};
use crate::state::{AppStateInit, DownloadProgress, IdleSweepTerminal, InstallStore, SessionStore};
use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
use axial_launcher::{LaunchSessionRecord, LaunchState, SessionId};
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

const MANAGED_STATE_FILE_NAME: &str = ".axial-lock.json";

fn managed_state_fixture_bytes(state: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 2,
        "state": state,
    }))
    .expect("serialize managed state fixture")
}

fn write_managed_state_fixture(mods_dir: &FsPath, state: &impl serde::Serialize) -> PathBuf {
    fs::create_dir_all(mods_dir).expect("create managed state fixture directory");
    let path = mods_dir.join(MANAGED_STATE_FILE_NAME);
    fs::write(&path, managed_state_fixture_bytes(state)).expect("write managed state fixture");
    path
}

struct RollbackFixture {
    id: String,
}

fn create_private_fixture_directory(root: &FsPath, path: &FsPath) {
    assert!(
        path.starts_with(root),
        "private fixture path must stay below its root"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .expect("create private fixture directory");
        for directory in path.ancestors().take_while(|directory| *directory != root) {
            let mode = fs::symlink_metadata(directory)
                .expect("read private fixture directory metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "fixture directory must be owner-only");
        }
    }

    #[cfg(not(unix))]
    fs::create_dir_all(path).expect("create private fixture directory");
}

fn write_rollback_fixture(
    mods_dir: &FsPath,
    id: &str,
    created_at: &str,
    state: &CompositionState,
    latest: bool,
) -> RollbackFixture {
    let rollback_dir = mods_dir.join(".axial-performance").join("rollback");
    let files_dir = rollback_dir.join("files");
    let history_dir = rollback_dir.join("history");
    create_private_fixture_directory(mods_dir, &files_dir);
    create_private_fixture_directory(mods_dir, &history_dir);
    create_private_fixture_directory(mods_dir, &rollback_dir.join("tmp"));

    let artifacts = state
        .installed_mods
        .iter()
        .enumerate()
        .map(|(index, installed)| {
            let stored_filename = format!("{id}-{index}.bin");
            fs::copy(
                mods_dir.join(&installed.filename),
                files_dir.join(&stored_filename),
            )
            .expect("copy rollback artifact fixture");
            serde_json::json!({
                "filename": installed.filename,
                "stored_filename": stored_filename,
                "project_id": installed.project_id,
                "version_id": installed.version_id,
                "ownership_class": installed.ownership_class,
                "sha512": installed.integrity.sha512,
            })
        })
        .collect::<Vec<_>>();
    let snapshot = serde_json::json!({
        "id": id,
        "schema_version": 3,
        "created_at": created_at,
        "target": {
            "kind": "managed_composition",
            "state": state,
        },
        "artifacts": artifacts,
    });
    let metadata = serde_json::to_vec_pretty(&snapshot).expect("serialize rollback fixture");
    fs::write(history_dir.join(format!("{id}.json")), &metadata)
        .expect("write rollback history fixture");
    if latest {
        fs::write(rollback_dir.join("latest.json"), metadata)
            .expect("write latest rollback fixture");
    }

    RollbackFixture { id: id.to_string() }
}

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
    let request = state.try_admit_request().expect("admit health request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim health producer");
    performance_health(&state, query, &producer).await.map(Json)
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

async fn collect_install_events(
    state: &AppState,
    install_id: impl ToString,
) -> Vec<DownloadProgress> {
    let install_id = install_id.to_string();
    let (snapshot, mut receiver) = state
        .installs()
        .subscribe_records(&install_id)
        .await
        .expect("install session should exist");
    let mut events = snapshot
        .latest
        .map(|record| vec![record.progress])
        .unwrap_or_default();
    if snapshot.done || events.iter().any(|event| event.done) {
        return events;
    }

    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("progress event should arrive")
            .expect("progress receiver should stay open");
        let terminal = event.progress.done;
        events.push(event.progress);
        if terminal {
            return events;
        }
    }
}

async fn wait_for_integrity_idle(state: &AppState, expected: bool) {
    let mut idle = state.subscribe_integrity_idle();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if idle.borrow_and_update().is_stably_idle() == expected {
                return;
            }
            idle.changed()
                .await
                .expect("integrity idle state remains open");
        }
    })
    .await
    .expect("integrity idle state settles");
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
    cleanup_root: bool,
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

        Self {
            state,
            root,
            cleanup_root: true,
        }
    }

    fn add_instance(&self, name: &str, version_id: &str) -> String {
        self.state
            .instances()
            .insert_for_test(name.to_string(), version_id.to_string())
            .expect("add instance")
            .id
    }

    async fn add_persisted_instance(&self, name: &str, version_id: &str) -> String {
        let instance = crate::state::new_instance(
            axial_config::generate_instance_id(),
            name.to_string(),
            version_id.to_string(),
            String::new(),
            String::new(),
        );
        let foreground = self
            .state
            .register_integrity_foreground()
            .expect("register persisted fixture foreground")
            .wait_for_settlement()
            .await;
        self.state
            .create_instance(&foreground, instance, None)
            .await
            .expect("persist fixture instance")
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
                "axialMaterialized": true,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }))
            .expect("serialize version"),
        )
        .expect("write version json");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write version jar");
    }

    fn write_vanilla_version(&self, version_id: &str) {
        let version_dir = self.root.join("library").join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("create vanilla version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "jre-legacy", "majorVersion": 8 },
                "libraries": []
            }))
            .expect("serialize vanilla version"),
        )
        .expect("write vanilla version json");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write vanilla version jar");
    }

    fn preserve_root_for_restart(&mut self) -> PathBuf {
        self.cleanup_root = false;
        self.root.clone()
    }
}

fn fabric_version_id(minecraft_version: &str) -> String {
    axial_minecraft::installed_version_id_for(
        axial_minecraft::LoaderComponentId::Fabric,
        minecraft_version,
        "0.16.10",
    )
    .expect("valid Fabric test identity")
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
        if self.cleanup_root {
            let _ = fs::remove_dir_all(&self.root);
        }
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

fn test_launch_record(session_id: &str, instance_id: &str) -> LaunchSessionRecord {
    LaunchSessionRecord {
        session_id: SessionId(session_id.to_string()),
        instance_id: instance_id.to_string(),
        version_id: "1.20.4-fabric".to_string(),
        launched_at: Some("2026-07-11T00:00:00.000Z".to_string()),
        benchmark: None,
        state: LaunchState::Queued,
        pid: None,
        process_started_at_ms: None,
        boot_completed_at_ms: None,
        boot_duration_ms: None,
        priority: None,
        exit_code: None,
        command: Vec::new(),
        java_path: None,
        natives_dir: None,
        failure: None,
        crash_evidence: None,
        healing: None,
        guardian: None,
        outcome: None,
        stages: Vec::new(),
    }
}

fn test_paths(root: &std::path::Path) -> AppPaths {
    AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
}

fn build_test_state(
    root: &FsPath,
    remote_rules_url: Option<String>,
    remote_rules_public_key: Option<String>,
) -> AppState {
    let paths = test_paths(root);
    let config = Arc::new(
        ConfigStore::from_config(
            paths.clone(),
            AppConfig {
                library_dir: paths.library_dir().to_string_lossy().to_string(),
                ..AppConfig::default()
            },
        )
        .expect("configure library dir"),
    );
    let instances = Arc::new(InstanceStore::load_for_startup(paths.clone()).store);
    AppState::new(AppStateInit {
        app_name: "Axial".to_string(),
        version: "test".to_string(),
        config,
        instances,
        installs: Arc::new(InstallStore::new()),
        sessions: Arc::new(SessionStore::new()),
        performance: Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                paths.performance_dir(),
                remote_rules_url,
                remote_rules_public_key,
            )
            .expect("performance manager"),
        ),
        startup_warnings: Vec::new(),
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

fn test_operation_payload() -> PerformanceOperationPayload {
    PerformanceOperationPayload {
        game_version: None,
        loader: None,
        mode: None,
        rollback_id: None,
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
    mut installed_mods: Vec<InstalledMod>,
) -> CompositionState {
    installed_mods.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    let declarative = axial_performance::CompositionPlan {
        composition_id: composition_id.to_string(),
        family: axial_performance::types::VersionFamily::F,
        loader: "fabric".to_string(),
        mode: PerformanceMode::Managed,
        tier: CompositionTier::Core,
        mods: installed_mods
            .iter()
            .filter(|installed| installed.role == axial_performance::ManagedArtifactRole::Root)
            .map(|installed| axial_performance::types::ManagedMod {
                artifact_id: installed.project_id.clone(),
                project_id: installed.project_id.clone(),
                slug: installed.project_id.clone(),
                name: installed.project_id.clone(),
                condition: axial_performance::types::ModCondition::Always,
                version_range: String::new(),
                exact_game_versions: Vec::new(),
                hardware_req: None,
                mutual_exclusions: Vec::new(),
            })
            .collect(),
        jvm_preset: String::new(),
        warnings: Vec::new(),
        fallback_reason: String::new(),
    };
    let pins = installed_mods
        .iter()
        .map(|installed| {
            axial_performance::ManagedArtifactPin::new(
                &installed.project_id,
                &installed.version_id,
                &installed.filename,
                format!(
                    "https://cdn.modrinth.com/data/{}/versions/{}/{}",
                    installed.project_id, installed.version_id, installed.filename
                ),
                installed.size,
                &installed.integrity.sha512,
                installed.role,
            )
            .expect("valid managed artifact fixture")
        })
        .collect();
    let sealed = axial_performance::ManagedCompositionInstallPlan::seal(
        declarative,
        "1.20.4",
        "fabric",
        pins,
        Vec::new(),
    )
    .expect("seal managed composition fixture");
    CompositionState {
        composition_id: composition_id.to_string(),
        family: axial_performance::types::VersionFamily::F,
        tier: CompositionTier::Core,
        game_version: "1.20.4".to_string(),
        loader: "fabric".to_string(),
        graph_sha512: sealed.graph_digest().to_string(),
        dependency_edges: Vec::new(),
        installed_mods,
        installed_at: "2026-05-30T00:00:00Z".to_string(),
    }
}

fn test_installed_mod(project_id: &str, filename: &str) -> InstalledMod {
    InstalledMod {
        project_id: project_id.to_string(),
        version_id: "NFkjnzWE".to_string(),
        filename: filename.to_string(),
        role: axial_performance::ManagedArtifactRole::Root,
        size: 1,
        ownership_class: axial_performance::OwnershipClass::CompositionManaged,
        source: test_modrinth_source(),
        integrity: axial_performance::ManagedArtifactIntegrity {
            sha512: "0".repeat(128),
        },
    }
}

fn test_modrinth_source() -> axial_performance::ManagedArtifactSource {
    axial_performance::ManagedArtifactSource {
        provider: axial_performance::ManagedArtifactProvider::Modrinth,
    }
}
