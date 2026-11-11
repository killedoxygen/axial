use super::InstallError;
use super::artifact::{ManagedArtifactStage, state_from_plan};
use super::storage::TestManagedStorage;
use super::{ManagedArtifactPin, ManagedCompositionInstallPlan, ManagedDependencyEdge};
use super::{commit_staged_graph, managed_stage_selection, remove_managed_transaction};
use crate::performance::model::{
    CompositionPlan, CompositionState, CompositionTier, InstalledMod, ManagedArtifactIntegrity,
    ManagedArtifactProvider, ManagedArtifactRole, ManagedArtifactSource, ManagedMod, ModCondition,
    OwnershipClass, PerformanceMode, VersionFamily,
};
use axial_minecraft::download::{
    RetryPolicy, TransferClient, TransferClientConfig, TransferOrigin,
};
use sha2::{Digest, Sha512};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct FakeOwnedStage {
    installed: InstalledMod,
    bytes: Vec<u8>,
    fail_publication: bool,
}

impl ManagedArtifactStage for FakeOwnedStage {
    fn installed(&self) -> &InstalledMod {
        &self.installed
    }

    fn publish_create_new(
        self,
        destination: &super::storage::ManagedStorageDirectory,
    ) -> Result<(), InstallError> {
        if self.fail_publication {
            return Err(InstallError::Io(std::io::Error::other(
                "injected managed publication failure",
            )));
        }
        let addition =
            super::state::prepare_managed_artifact_addition(destination, &self.installed)?;
        destination.create_file_create_new(Path::new(&self.installed.filename), &self.bytes)?;
        super::state::publish_managed_artifact_addition(destination, &self.installed, &addition)
            .map_err(Into::into)
    }
}

#[test]
fn changed_graph_requests_provider_only_for_the_changed_pin() {
    let root = test_root("changed-pin-provider-selection");
    let storage = TestManagedStorage::new(&root);
    let (_, previous) = graph(b"root", b"dependency");
    write_graph_files(&root, b"root", b"dependency");
    super::state::save_state(storage.directory(), &previous).expect("save previous state");
    let (next_plan, next) = graph(b"replacement-root", b"dependency");

    let selection =
        managed_stage_selection(storage.directory(), &next_plan).expect("select changed pins");

    assert!(selection.exact_state.is_none());
    assert_eq!(selection.pins.len(), 1, "one provider request is required");
    assert_eq!(selection.pins[0].filename(), "root.jar");

    let changed_stage = fake_stages(&next_plan, false)
        .into_iter()
        .filter(|stage| stage.installed.filename == "root.jar")
        .collect();
    commit_staged_graph(storage.directory(), Some(&previous), &next, changed_stage)
        .expect("commit changed pin with retained dependency");
    assert_graph_files(&root, b"replacement-root", b"dependency");
}

#[test]
fn changed_graph_revalidates_retained_pin_before_commit_effects() {
    let root = test_root("changed-pin-precommit-revalidation");
    let storage = TestManagedStorage::new(&root);
    let (_, previous) = graph(b"root", b"dependency");
    write_graph_files(&root, b"root", b"dependency");
    super::state::save_state(storage.directory(), &previous).expect("save previous state");
    let lock_before = fs::read(root.join(".axial-lock.json")).expect("lock bytes");
    let (next_plan, next) = graph(b"replacement-root", b"dependency");
    let changed_stage = fake_stages(&next_plan, false)
        .into_iter()
        .filter(|stage| stage.installed.filename == "root.jar")
        .collect();
    fs::write(root.join("dependency.jar"), b"changed-after-selection")
        .expect("replace retained dependency");

    commit_staged_graph(storage.directory(), Some(&previous), &next, changed_stage)
        .expect_err("changed retained pin must abort before commit");

    assert_eq!(fs::read(root.join("root.jar")).expect("old root"), b"root");
    assert_eq!(
        fs::read(root.join("dependency.jar")).expect("external replacement"),
        b"changed-after-selection"
    );
    assert_eq!(
        fs::read(root.join(".axial-lock.json")).expect("unchanged lock"),
        lock_before
    );
    assert!(!root.join(".axial-performance").exists());
}

#[test]
fn dependency_complete_graph_commits_and_rolls_back_to_absence() {
    let root = test_root("complete-graph");
    let storage = TestManagedStorage::new(&root);
    let (plan, state) = graph(b"root", b"dependency");
    super::state::save_absent_rollback_snapshot(storage.directory()).expect("snapshot absence");

    commit_staged_graph(storage.directory(), None, &state, fake_stages(&plan, false))
        .expect("commit full graph");

    assert_graph_files(&root, b"root", b"dependency");
    assert_eq!(
        super::state::load_state(storage.directory())
            .expect("load state")
            .expect("managed state"),
        state
    );
    let snapshot = super::state::load_rollback_snapshot(storage.directory())
        .expect("load rollback")
        .expect("absence rollback");
    assert!(matches!(
        super::state::restore_rollback_snapshot(storage.directory(), &snapshot)
            .expect("restore absence"),
        super::state::ManagedRollbackOutcome::ManagedStateAbsent
    ));
    assert!(!root.join("root.jar").exists());
    assert!(!root.join("dependency.jar").exists());
}

#[tokio::test]
async fn provider_stage_failure_has_zero_managed_or_snapshot_effect() {
    let instance = test_root("provider-failure-instance");
    let mods = instance.join("mods");
    fs::create_dir(&mods).expect("create mods");
    let mods_storage = TestManagedStorage::new(&mods);
    let (_, previous) = graph(b"root", b"dependency");
    write_graph_files(&mods, b"root", b"dependency");
    super::state::save_state(mods_storage.directory(), &previous).expect("save previous state");
    let lock_before = fs::read(mods.join(".axial-lock.json")).expect("lock bytes");
    let failed_plan = graph_plan_at(
        b"replacement-root",
        b"replacement-dependency",
        "https://127.0.0.1:1",
    );
    let instance_anchor = TestManagedStorage::new(&instance);

    let callback_called = Arc::new(AtomicBool::new(false));
    let callback_probe = callback_called.clone();
    let error = super::install_graph(
        &failed_plan,
        test_transfer_resolver(),
        instance_anchor.directory(),
        move || async move {
            callback_probe.store(true, Ordering::Release);
            Ok::<(), ()>(())
        },
    )
    .await
    .expect_err("provider stage must fail");

    assert!(matches!(
        error,
        super::ManagedInstallExecutionError::Mutation(_)
    ));
    assert!(!callback_called.load(Ordering::Acquire));
    assert_graph_files(&mods, b"root", b"dependency");
    assert_eq!(
        fs::read(mods.join(".axial-lock.json")).expect("lock bytes"),
        lock_before
    );
    assert!(
        super::state::load_rollback_snapshot(mods_storage.directory())
            .expect("load rollback")
            .is_none()
    );
    drop(mods_storage);
    drop(instance_anchor);

    let fresh_instance = test_root("provider-failure-fresh-instance");
    let fresh_mods = fresh_instance.join("mods");
    let fresh_instance_anchor = TestManagedStorage::new(&fresh_instance);
    super::install_graph(
        &failed_plan,
        test_transfer_resolver(),
        fresh_instance_anchor.directory(),
        || async { Ok::<(), ()>(()) },
    )
    .await
    .expect_err("fresh provider stage must fail");
    assert!(fresh_mods.is_dir());
    let fresh_mods_storage = fresh_instance_anchor
        .directory()
        .open_child("mods")
        .expect("open fresh mods after candidate rollback")
        .expect("candidate intent creates the managed mods root");
    assert_eq!(
        super::state::load_state(&fresh_mods_storage).expect("load fresh state"),
        None
    );
    super::state::prove_managed_storage_recovered(&fresh_mods_storage, None)
        .expect("provider failure settles every candidate intent");
}

#[tokio::test]
async fn managed_graph_transfers_two_candidates_before_publication() {
    let root_bytes = b"downloaded-root";
    let dependency_bytes = b"downloaded-dependency";
    let (base_url, server) = start_artifact_server(root_bytes, dependency_bytes).await;

    let instance = test_root("concurrent-candidate-transfers");
    let instance_anchor = TestManagedStorage::new(&instance);
    let plan = graph_plan_at(root_bytes, dependency_bytes, &base_url);

    let outcome = super::install_graph(
        &plan,
        test_transfer_resolver(),
        instance_anchor.directory(),
        || async { Ok::<(), ()>(()) },
    )
    .await
    .expect("download and publish the complete graph");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("transfer fixture completed")
        .expect("transfer fixture task")
        .expect("serve transfer fixtures");

    let mods = instance.join("mods");
    assert!(outcome.target_changed());
    assert_graph_files(&mods, root_bytes, dependency_bytes);
    let mods_storage = instance_anchor
        .directory()
        .open_child("mods")
        .expect("open installed mods")
        .expect("managed mods directory");
    let state = super::state::load_state(&mods_storage)
        .expect("load installed state")
        .expect("installed state");
    assert_eq!(state, outcome.into_state());
    super::state::prove_managed_storage_recovered(&mods_storage, Some(&state))
        .expect("all transfer candidates and intents settled");
}

#[tokio::test]
async fn rejected_target_checkpoint_cleans_every_verified_candidate() {
    let root_bytes = b"downloaded-root";
    let dependency_bytes = b"downloaded-dependency";
    let (base_url, server) = start_artifact_server(root_bytes, dependency_bytes).await;
    let instance = test_root("rejected-target-checkpoint");
    let instance_anchor = TestManagedStorage::new(&instance);
    let plan = graph_plan_at(root_bytes, dependency_bytes, &base_url);

    let error = super::install_graph(
        &plan,
        test_transfer_resolver(),
        instance_anchor.directory(),
        || async { Err::<(), _>("checkpoint rejected") },
    )
    .await
    .expect_err("target checkpoint must reject publication");
    assert!(matches!(
        error,
        super::ManagedInstallExecutionError::BeforeTargetEffect("checkpoint rejected")
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("transfer fixture completed")
        .expect("transfer fixture task")
        .expect("serve transfer fixtures");

    let mods = instance_anchor
        .directory()
        .open_child("mods")
        .expect("open rejected mods")
        .expect("candidate preparation creates mods");
    assert_eq!(
        super::state::load_state(&mods).expect("load rejected state"),
        None
    );
    assert!(
        super::state::load_rollback_snapshot(&mods)
            .expect("load absence rollback")
            .is_some()
    );
    super::state::prove_managed_storage_recovered(&mods, None)
        .expect("checkpoint rejection settles candidate state");
    assert!(!instance.join("mods/root.jar").exists());
    assert!(!instance.join("mods/dependency.jar").exists());
}

#[tokio::test]
async fn exact_graph_noop_skips_the_target_effect_boundary() {
    let instance = test_root("exact-noop-boundary");
    let mods = instance.join("mods");
    fs::create_dir(&mods).expect("create mods");
    let mods_storage = TestManagedStorage::new(&mods);
    let (plan, state) = graph(b"root", b"dependency");
    write_graph_files(&mods, b"root", b"dependency");
    super::state::save_state(mods_storage.directory(), &state).expect("save exact managed state");
    let instance_anchor = TestManagedStorage::new(&instance);
    let callback_called = Arc::new(AtomicBool::new(false));
    let callback_probe = callback_called.clone();

    let outcome = super::install_graph(
        &plan,
        test_transfer_resolver(),
        instance_anchor.directory(),
        move || async move {
            callback_probe.store(true, Ordering::Release);
            Ok::<(), ()>(())
        },
    )
    .await
    .expect("exact graph is a no-op");

    assert!(!outcome.target_changed());
    assert!(!callback_called.load(Ordering::Acquire));
    assert!(
        super::state::load_rollback_snapshot(mods_storage.directory())
            .expect("load rollback")
            .is_none()
    );
}

#[tokio::test]
async fn target_effect_boundary_follows_snapshot_and_precedes_managed_mutation() {
    let instance = test_root("snapshot-effect-boundary");
    let mods = instance.join("mods");
    fs::create_dir(&mods).expect("create mods");
    let mods_storage = TestManagedStorage::new(&mods);
    let (_, previous) = graph(b"root", b"dependency");
    write_graph_files(&mods, b"root", b"dependency");
    super::state::save_state(mods_storage.directory(), &previous)
        .expect("save previous managed state");
    let next_plan = graph_plan_named_at(
        "replacement-core",
        b"root",
        b"dependency",
        "https://cdn.example.invalid",
    );
    let instance_anchor = TestManagedStorage::new(&instance);
    let callback_mods = mods.clone();
    let callback_storage = mods_storage.directory().clone();

    let outcome = super::install_graph(
        &next_plan,
        test_transfer_resolver(),
        instance_anchor.directory(),
        move || async move {
            assert!(
                super::state::load_rollback_snapshot(&callback_storage)
                    .expect("load rollback at effect boundary")
                    .is_some()
            );
            assert_eq!(
                super::state::load_state(&callback_storage)
                    .expect("load state at effect boundary")
                    .expect("previous state")
                    .composition_id,
                "core"
            );
            assert_graph_files(&callback_mods, b"root", b"dependency");
            Ok::<(), ()>(())
        },
    )
    .await
    .expect("commit replacement composition");

    assert!(outcome.target_changed());
    assert_eq!(outcome.into_state().composition_id, "replacement-core");
}

#[test]
fn mid_commit_failure_restores_exact_prior_graph() {
    let root = test_root("mid-commit-rollback");
    let storage = TestManagedStorage::new(&root);
    let (_, previous) = graph(b"root", b"dependency");
    write_graph_files(&root, b"root", b"dependency");
    super::state::save_state(storage.directory(), &previous).expect("save previous state");
    let lock_before = fs::read(root.join(".axial-lock.json")).expect("lock bytes");
    super::state::save_rollback_snapshot(storage.directory(), &previous).expect("save rollback");

    let (replacement_plan, replacement) = graph(b"replacement-root", b"replacement-dependency");
    let mut stages = fake_stages(&replacement_plan, false);
    stages[1].fail_publication = true;
    commit_staged_graph(storage.directory(), Some(&previous), &replacement, stages)
        .expect_err("injected mid-commit failure");
    let snapshot = super::state::load_rollback_snapshot(storage.directory())
        .expect("load rollback")
        .expect("rollback");
    super::state::restore_rollback_snapshot(storage.directory(), &snapshot)
        .expect("restore prior graph");

    assert_graph_files(&root, b"root", b"dependency");
    assert_eq!(
        fs::read(root.join(".axial-lock.json")).expect("lock bytes"),
        lock_before
    );
    assert_eq!(
        super::state::load_state(storage.directory())
            .expect("load state")
            .expect("prior state"),
        previous
    );
}

#[test]
fn commit_collision_never_claims_or_removes_user_owned_destination() {
    let root = test_root("user-destination-collision");
    let storage = TestManagedStorage::new(&root);
    let (plan, state) = graph(b"root", b"dependency");
    fs::write(root.join("root.jar"), b"user-owned").expect("write user file");

    commit_staged_graph(storage.directory(), None, &state, fake_stages(&plan, false))
        .expect_err("untracked destination must block publication");
    super::state::reconcile_managed_addition_obligations(storage.directory(), None)
        .expect("discard untracked managed obligations");

    assert_eq!(
        fs::read(root.join("root.jar")).expect("user destination preserved"),
        b"user-owned"
    );
    assert!(!root.join("dependency.jar").exists());
    assert!(
        super::state::load_state(storage.directory())
            .expect("load state")
            .is_none()
    );
}

#[test]
fn full_graph_remove_and_rollback_preserve_dependencies() {
    let root = test_root("remove-rollback");
    let storage = TestManagedStorage::new(&root);
    let (plan, state) = graph(b"root", b"dependency");
    commit_staged_graph(storage.directory(), None, &state, fake_stages(&plan, false))
        .expect("install graph");

    remove_managed_transaction(storage.directory()).expect("remove graph");
    assert!(!root.join("root.jar").exists());
    assert!(!root.join("dependency.jar").exists());
    let snapshot = super::state::load_rollback_snapshot(storage.directory())
        .expect("load rollback")
        .expect("rollback");
    assert!(matches!(
        super::state::restore_rollback_snapshot(storage.directory(), &snapshot)
            .expect("restore graph"),
        super::state::ManagedRollbackOutcome::ManagedComposition(_)
    ));
    assert_graph_files(&root, b"root", b"dependency");
}

fn graph(root: &[u8], dependency: &[u8]) -> (ManagedCompositionInstallPlan, CompositionState) {
    let plan = graph_plan(root, dependency);
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    (plan, state)
}

fn graph_plan(root: &[u8], dependency: &[u8]) -> ManagedCompositionInstallPlan {
    graph_plan_at(root, dependency, "https://cdn.example.invalid")
}

fn test_transfer_resolver() -> super::ManagedArtifactTransferResolver {
    super::ManagedArtifactTransferResolver::new(
        |url| async move {
            let origin = if url.scheme() == "http" {
                TransferOrigin::from_loopback_http_for_test_support(&url)
            } else {
                TransferOrigin::from_url(&url)
            }
            .map_err(std::io::Error::other)?;
            let config = TransferClientConfig::bounded(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(2),
                vec![origin],
            )
            .map_err(std::io::Error::other)?;
            TransferClient::build(config).map_err(std::io::Error::other)
        },
        RetryPolicy::none(),
    )
}

async fn start_artifact_server(
    root: &'static [u8],
    dependency: &'static [u8],
) -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind managed transfer fixture");
    let address = listener.local_addr().expect("fixture address");
    let server = tokio::spawn(async move {
        let mut handlers = Vec::with_capacity(2);
        for _ in 0..2 {
            let (stream, _) = listener.accept().await?;
            handlers.push(tokio::spawn(serve_artifact(stream, root, dependency)));
        }
        for handler in handlers {
            handler.await.map_err(std::io::Error::other)??;
        }
        Ok(())
    });
    (format!("http://{address}"), server)
}

async fn serve_artifact(
    mut stream: TcpStream,
    root: &'static [u8],
    dependency: &'static [u8],
) -> std::io::Result<()> {
    let mut request = Vec::with_capacity(512);
    loop {
        let mut buffer = [0_u8; 256];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > 4096 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed transfer fixture request exceeded its bound",
            ));
        }
    }
    let body = if request.starts_with(b"GET /root.jar ") {
        root
    } else if request.starts_with(b"GET /dependency.jar ") {
        dependency
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed transfer fixture received an unexpected target",
        ));
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

fn graph_plan_at(root: &[u8], dependency: &[u8], base_url: &str) -> ManagedCompositionInstallPlan {
    graph_plan_named_at("core", root, dependency, base_url)
}

fn graph_plan_named_at(
    composition_id: &str,
    root: &[u8],
    dependency: &[u8],
    base_url: &str,
) -> ManagedCompositionInstallPlan {
    let declarative = CompositionPlan {
        composition_id: composition_id.to_string(),
        family: VersionFamily::F,
        loader: "fabric".to_string(),
        mode: PerformanceMode::Managed,
        tier: CompositionTier::Core,
        mods: vec![ManagedMod {
            artifact_id: "root".to_string(),
            project_id: "AANobbMI".to_string(),
            slug: String::new(),
            name: "Root".to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }],
        jvm_preset: String::new(),
        warnings: Vec::new(),
        fallback_reason: String::new(),
    };
    ManagedCompositionInstallPlan::seal(
        declarative,
        "1.21.11",
        "fabric",
        vec![
            pin(
                "P7dR8mSH",
                "1234abcd",
                "dependency.jar",
                dependency,
                ManagedArtifactRole::RequiredDependency,
                base_url,
            ),
            pin(
                "AANobbMI",
                "NFkjnzWE",
                "root.jar",
                root,
                ManagedArtifactRole::Root,
                base_url,
            ),
        ],
        vec![
            ManagedDependencyEdge::new("AANobbMI", "P7dR8mSH", "1234abcd")
                .expect("dependency edge"),
        ],
    )
    .expect("seal graph")
}

fn pin(
    project_id: &str,
    version_id: &str,
    filename: &str,
    body: &[u8],
    role: ManagedArtifactRole,
    base_url: &str,
) -> ManagedArtifactPin {
    ManagedArtifactPin::new(
        project_id,
        version_id,
        filename,
        format!("{base_url}/{filename}"),
        body.len() as u64,
        hex::encode(Sha512::digest(body)),
        role,
    )
    .expect("artifact pin")
}

fn installed_from_plan(plan: &ManagedCompositionInstallPlan) -> Vec<InstalledMod> {
    plan.pins()
        .iter()
        .map(|pin| InstalledMod {
            project_id: pin.project_id().to_string(),
            version_id: pin.version_id().to_string(),
            filename: pin.filename().to_string(),
            role: pin.role(),
            size: pin.size(),
            ownership_class: OwnershipClass::CompositionManaged,
            source: ManagedArtifactSource {
                provider: ManagedArtifactProvider::Modrinth,
            },
            integrity: ManagedArtifactIntegrity {
                sha512: pin.sha512().to_string(),
            },
        })
        .collect()
}

fn fake_stages(
    plan: &ManagedCompositionInstallPlan,
    fail_publication: bool,
) -> Vec<FakeOwnedStage> {
    installed_from_plan(plan)
        .into_iter()
        .map(|installed| FakeOwnedStage {
            bytes: match installed.filename.as_str() {
                "root.jar" if installed.size == b"root".len() as u64 => b"root".to_vec(),
                "dependency.jar" if installed.size == b"dependency".len() as u64 => {
                    b"dependency".to_vec()
                }
                "root.jar" => b"replacement-root".to_vec(),
                "dependency.jar" => b"replacement-dependency".to_vec(),
                _ => unreachable!("test graph filename"),
            },
            installed,
            fail_publication,
        })
        .collect()
}

fn write_graph_files(root: &Path, root_bytes: &[u8], dependency_bytes: &[u8]) {
    fs::write(root.join("root.jar"), root_bytes).expect("write root");
    fs::write(root.join("dependency.jar"), dependency_bytes).expect("write dependency");
}

fn assert_graph_files(root: &Path, root_bytes: &[u8], dependency_bytes: &[u8]) {
    assert_eq!(fs::read(root.join("root.jar")).expect("root"), root_bytes);
    assert_eq!(
        fs::read(root.join("dependency.jar")).expect("dependency"),
        dependency_bytes
    );
}

fn test_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir()
        .canonicalize()
        .expect("canonical temporary parent")
        .join(format!(
            "axial-performance-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ))
        .join("managed");
    fs::create_dir_all(&path).expect("create test root");
    path
}
