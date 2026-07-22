use super::artifact::{ManagedArtifactStage, state_from_plan};
use super::model::InstallError;
use super::mutation::{commit_staged_graph, managed_stage_selection, remove_managed_transaction};
use super::{
    ManagedArtifactPin, ManagedArtifactRole, ManagedCompositionInstallPlan, ManagedDependencyEdge,
};
use crate::types::{
    CompositionPlan, CompositionTier, InstalledMod, ManagedArtifactIntegrity,
    ManagedArtifactProvider, ManagedArtifactSource, ManagedMod, ModCondition, OwnershipClass,
    PerformanceMode, VersionFamily,
};
use axial_fs::{
    FileCreateObligation, FileCreateOutcome, FileCreateResolution, FilePromotionObligation,
    FilePromotionOutcome, FilePromotionResolution, LeafName, RootSession,
    RootSessionAcquireOutcome, StageDiscardObligation, StageDiscardOutcome,
    StageDiscardResolution,
};
use sha2::{Digest, Sha512};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        destination: &crate::storage::ManagedStorageDirectory,
        filename: &str,
    ) -> Result<(), InstallError> {
        if self.fail_publication {
            return Err(InstallError::Io(std::io::Error::other(
                "injected managed publication failure",
            )));
        }
        publish_test_bytes_create_new(destination, filename, &self.bytes)
    }
}

#[test]
fn sealed_graph_state_retains_required_dependency_identity() {
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));

    assert_eq!(state.graph_sha512, plan.graph_digest());
    assert_eq!(state.dependency_edges.len(), 1);
    assert_eq!(state.installed_mods.len(), 2);
    assert!(
        state
            .installed_mods
            .iter()
            .any(|installed| installed.role == ManagedArtifactRole::RequiredDependency)
    );
    crate::install::plan::validate_state_graph(&state).expect("valid sealed state graph");
}

#[test]
fn managed_authority_consumes_injected_instances_hierarchy() {
    let instances_root = test_root("managed-authority-injected-hierarchy");
    let storage = anchor(&instances_root);
    let manager = Arc::new(super::PerformanceManager::new().expect("performance manager"));

    let authority = manager
        .claim_managed_authority(storage.directory().directory().clone())
        .expect("claim injected managed authority");

    assert!(instances_root.is_dir());
    assert_eq!(
        authority
            .identify("0000000000000001")
            .expect("identify beneath fresh hierarchy")
            .instance_id(),
        "0000000000000001"
    );
    drop(authority);
    drop(storage);
    fs::remove_dir_all(instances_root).expect("remove injected authority hierarchy");
}

#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn managed_authority_refuses_a_live_owner_for_a_replaced_instance() {
    let container = test_root("managed-authority-replaced-instance");
    let instances_root = container.join("instances");
    let instance_id = "0000000000000001";
    let instance = instances_root.join(instance_id);
    let displaced = instances_root.join("displaced-instance");
    fs::create_dir_all(&instance).expect("create original instance");
    let storage = anchor(&instances_root);
    let manager = Arc::new(super::PerformanceManager::new().expect("performance manager"));
    let authority = manager
        .claim_managed_authority(storage.directory().directory().clone())
        .expect("claim managed authority");
    let identity = authority.identify(instance_id).expect("identify instance");
    let stale = authority
        .bind_instance_effect_authority(&identity)
        .await
        .expect("bind original instance effect authority");

    if let Err(error) = fs::rename(&instance, &displaced) {
        assert!(cfg!(windows), "instance replacement failed: {error}");
        drop(stale);
        drop(authority);
        drop(storage);
        fs::remove_dir_all(container).expect("remove replacement test root");
        return;
    }
    fs::create_dir(&instance).expect("create replacement instance");

    let error = match authority.bind_instance_effect_authority(&identity).await {
        Ok(_) => panic!("a live stale owner must not bind to the replacement"),
        Err(error) => error,
    };
    match error {
        super::ManagedMutationError::Definite(InstallError::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        }
        error => panic!("unexpected replacement rejection: {error}"),
    }

    drop(stale);
    let fresh = authority
        .bind_instance_effect_authority(&identity)
        .await
        .expect("bind a fresh owner after the stale owner drops");
    drop(fresh);
    drop(authority);
    drop(storage);
    fs::remove_dir_all(container).expect("remove replacement test root");
}

#[test]
fn exact_healthy_graph_skips_provider_and_snapshot_work() {
    let root = test_root("healthy-noop");
    let storage = anchor(&root);
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    write_graph_files(&root, b"root", b"dependency");
    crate::state::save_state(storage.directory(), &state).expect("save state");

    let admitted = managed_stage_selection(storage.directory(), &plan)
        .expect("inspect exact graph")
        .exact_state
        .expect("healthy graph");

    assert_eq!(admitted, state);
    assert!(
        crate::state::load_rollback_snapshot(storage.directory())
            .expect("load rollback")
            .is_none()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn changed_graph_requests_provider_only_for_the_changed_pin() {
    let root = test_root("changed-pin-provider-selection");
    let storage = anchor(&root);
    let previous_plan = graph_plan(b"root", b"dependency");
    let previous = state_from_plan(&previous_plan, installed_from_plan(&previous_plan));
    write_graph_files(&root, b"root", b"dependency");
    crate::state::save_state(storage.directory(), &previous).expect("save previous state");
    let next_plan = graph_plan(b"replacement-root", b"dependency");

    let selection = managed_stage_selection(storage.directory(), &next_plan).expect("select changed pins");

    assert!(selection.exact_state.is_none());
    assert_eq!(selection.pins.len(), 1, "one provider request is required");
    assert_eq!(selection.pins[0].filename(), "root.jar");

    let next = state_from_plan(&next_plan, installed_from_plan(&next_plan));
    let changed_stage = fake_stages(&next_plan, false)
        .into_iter()
        .filter(|stage| stage.installed.filename == "root.jar")
        .collect();
    commit_staged_graph(storage.directory(), Some(&previous), &next, changed_stage)
        .expect("commit changed pin with retained dependency");
    assert_graph_files(&root, b"replacement-root", b"dependency");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn changed_graph_revalidates_retained_pin_before_commit_effects() {
    let root = test_root("changed-pin-precommit-revalidation");
    let storage = anchor(&root);
    let previous_plan = graph_plan(b"root", b"dependency");
    let previous = state_from_plan(&previous_plan, installed_from_plan(&previous_plan));
    write_graph_files(&root, b"root", b"dependency");
    crate::state::save_state(storage.directory(), &previous).expect("save previous state");
    let lock_before = fs::read(root.join(".axial-lock.json")).expect("lock bytes");
    let next_plan = graph_plan(b"replacement-root", b"dependency");
    let next = state_from_plan(&next_plan, installed_from_plan(&next_plan));
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
    let _ = fs::remove_dir_all(root);
}

#[test]
fn dependency_complete_graph_commits_and_rolls_back_to_absence() {
    let root = test_root("complete-graph");
    let storage = anchor(&root);
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    crate::state::save_absent_rollback_snapshot(storage.directory()).expect("snapshot absence");

    commit_staged_graph(storage.directory(), None, &state, fake_stages(&plan, false))
        .expect("commit full graph");

    assert_graph_files(&root, b"root", b"dependency");
    assert_eq!(
        crate::state::load_state(storage.directory())
            .expect("load state")
            .expect("managed state"),
        state
    );
    let snapshot = crate::state::load_rollback_snapshot(storage.directory())
        .expect("load rollback")
        .expect("absence rollback");
    assert!(matches!(
        crate::state::restore_rollback_snapshot(storage.directory(), &snapshot).expect("restore absence"),
        crate::state::ManagedRollbackOutcome::ManagedStateAbsent
    ));
    assert!(!root.join("root.jar").exists());
    assert!(!root.join("dependency.jar").exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn provider_stage_failure_has_zero_managed_or_snapshot_effect() {
    let instance = test_root("provider-failure-instance");
    let mods = instance.join("mods");
    fs::create_dir(&mods).expect("create mods");
    let mods_storage = anchor(&mods);
    let previous_plan = graph_plan(b"root", b"dependency");
    let previous = state_from_plan(&previous_plan, installed_from_plan(&previous_plan));
    write_graph_files(&mods, b"root", b"dependency");
    crate::state::save_state(mods_storage.directory(), &previous).expect("save previous state");
    let lock_before = fs::read(mods.join(".axial-lock.json")).expect("lock bytes");
    let failed_plan = graph_plan_at(
        b"replacement-root",
        b"replacement-dependency",
        "https://127.0.0.1:1",
    );
    let manager = super::PerformanceManager::new().expect("manager");
    let instance_anchor = anchor(&instance);

    let callback_called = Arc::new(AtomicBool::new(false));
    let callback_probe = callback_called.clone();
    let error = manager
        .ensure_installed(
            &failed_plan,
            &reqwest::Client::new(),
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
        super::ManagedInstallExecutionError::Mutation {
            rollback_ready: false,
            ..
        }
    ));
    assert!(!callback_called.load(Ordering::Acquire));
    assert_graph_files(&mods, b"root", b"dependency");
    assert_eq!(
        fs::read(mods.join(".axial-lock.json")).expect("lock bytes"),
        lock_before
    );
    assert!(
        crate::state::load_rollback_snapshot(mods_storage.directory())
            .expect("load rollback")
            .is_none()
    );
    drop(mods_storage);
    drop(instance_anchor);
    let _ = fs::remove_dir_all(instance);

    let fresh_instance = test_root("provider-failure-fresh-instance");
    let fresh_mods = fresh_instance.join("mods");
    let fresh_instance_anchor = anchor(&fresh_instance);
    manager
        .ensure_installed(
            &failed_plan,
            &reqwest::Client::new(),
            fresh_instance_anchor.directory(),
            || async { Ok::<(), ()>(()) },
        )
        .await
        .expect_err("fresh provider stage must fail");
    assert!(!fresh_mods.exists());
    drop(fresh_instance_anchor);
    let _ = fs::remove_dir_all(fresh_instance);
}

#[tokio::test]
async fn exact_graph_noop_skips_the_target_effect_boundary() {
    let instance = test_root("exact-noop-boundary");
    let mods = instance.join("mods");
    fs::create_dir(&mods).expect("create mods");
    let mods_storage = anchor(&mods);
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    write_graph_files(&mods, b"root", b"dependency");
    crate::state::save_state(mods_storage.directory(), &state).expect("save exact managed state");
    let manager = super::PerformanceManager::new().expect("manager");
    let instance_anchor = anchor(&instance);
    let callback_called = Arc::new(AtomicBool::new(false));
    let callback_probe = callback_called.clone();

    let outcome = manager
        .ensure_installed(
            &plan,
            &reqwest::Client::new(),
            instance_anchor.directory(),
            move || async move {
                callback_probe.store(true, Ordering::Release);
                Ok::<(), ()>(())
            },
        )
        .await
        .expect("exact graph is a no-op");

    assert!(!outcome.target_changed());
    assert!(!outcome.rollback_ready());
    assert!(!callback_called.load(Ordering::Acquire));
    assert!(
        crate::state::load_rollback_snapshot(mods_storage.directory())
            .expect("load rollback")
            .is_none()
    );
    drop(mods_storage);
    drop(instance_anchor);
    let _ = fs::remove_dir_all(instance);
}

#[tokio::test]
async fn target_effect_boundary_follows_snapshot_and_precedes_managed_mutation() {
    let instance = test_root("snapshot-effect-boundary");
    let mods = instance.join("mods");
    fs::create_dir(&mods).expect("create mods");
    let mods_storage = anchor(&mods);
    let previous_plan = graph_plan(b"root", b"dependency");
    let previous = state_from_plan(&previous_plan, installed_from_plan(&previous_plan));
    write_graph_files(&mods, b"root", b"dependency");
    crate::state::save_state(mods_storage.directory(), &previous)
        .expect("save previous managed state");
    let next_plan = graph_plan_named_at(
        "replacement-core",
        b"root",
        b"dependency",
        "https://cdn.example.invalid",
    );
    let manager = super::PerformanceManager::new().expect("manager");
    let instance_anchor = anchor(&instance);
    let callback_mods = mods.clone();
    let callback_storage = mods_storage.directory().clone();

    let outcome = manager
        .ensure_installed(
            &next_plan,
            &reqwest::Client::new(),
            instance_anchor.directory(),
            move || async move {
                assert!(
                    crate::state::load_rollback_snapshot(&callback_storage)
                        .expect("load rollback at effect boundary")
                        .is_some()
                );
                assert_eq!(
                    crate::state::load_state(&callback_storage)
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
    assert!(outcome.rollback_ready());
    assert_eq!(outcome.into_state().composition_id, "replacement-core");
    drop(mods_storage);
    drop(instance_anchor);
    let _ = fs::remove_dir_all(instance);
}

#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn managed_authority_retains_state_and_evidence_across_ancestor_substitution() {
    let container = test_root("authority-ancestor-substitution");
    let root = container.join("instances");
    let moved = container.with_extension("moved");
    let instance_id = "0000000000000001";
    let mods = root.join(instance_id).join("mods");
    fs::create_dir_all(&mods).expect("create admitted mods");
    let mods_storage = anchor(&mods);
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    write_graph_files(&mods, b"root", b"dependency");
    fs::write(mods.join("user-evidence.jar"), b"user-owned").expect("write user jar evidence");
    crate::state::save_state(mods_storage.directory(), &state).expect("save admitted state");
    let manager = std::sync::Arc::new(super::PerformanceManager::new().expect("manager"));
    let storage = anchor(&root);
    let authority = manager
        .claim_managed_authority(storage.directory().directory().clone())
        .expect("claim authority");
    let identity = authority.identify(instance_id).expect("identify instance");

    #[cfg(target_os = "linux")]
    {
        fs::rename(&container, &moved).expect("rename admitted ancestor");
        fs::create_dir_all(root.join(instance_id).join("mods")).expect("create replacement tree");
    }
    #[cfg(windows)]
    fs::rename(&container, &moved).expect_err("authority blocks ancestor substitution");

    let effects = authority
        .bind_instance_effect_authority(&identity)
        .await
        .expect("bind instance effect authority");
    let proofs = authority
        .composition_managed_witness_proofs(&identity, &effects)
        .await
        .expect("observe through authority");
    assert_eq!(proofs.len(), 2);
    assert!(proofs.iter().any(|proof| {
        proof.matches_observation("root.jar", &hex::encode(Sha512::digest(b"root")))
    }));
    let resolved = authority
        .resolve_and_inspect(
            &identity,
            &effects,
            crate::types::ResolutionRequest {
                game_version: "1.21.11".to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: crate::types::HardwareProfile::default(),
                installed_mods: vec!["caller-supplied-evidence".to_string()],
            },
            || Ok(()),
        )
        .await
        .expect("resolve and inspect through retained authority");
    assert_eq!(resolved.inspection.state, Some(state));
    assert!(
        resolved
            .inspection
            .installed_mod_evidence
            .iter()
            .any(|evidence| evidence == "user-evidence")
    );
    assert!(
        !resolved
            .inspection
            .installed_mod_evidence
            .iter()
            .any(|evidence| evidence == "caller-supplied-evidence")
    );

    drop(effects);
    drop(authority);
    drop(storage);
    let _ = fs::remove_dir_all(container);
    let _ = fs::remove_dir_all(moved);
}

#[test]
fn mid_commit_failure_restores_exact_prior_graph() {
    let root = test_root("mid-commit-rollback");
    let storage = anchor(&root);
    let previous_plan = graph_plan(b"root", b"dependency");
    let previous = state_from_plan(&previous_plan, installed_from_plan(&previous_plan));
    write_graph_files(&root, b"root", b"dependency");
    crate::state::save_state(storage.directory(), &previous).expect("save previous state");
    let lock_before = fs::read(root.join(".axial-lock.json")).expect("lock bytes");
    crate::state::save_rollback_snapshot(storage.directory(), &previous).expect("save rollback");

    let replacement_plan = graph_plan(b"replacement-root", b"replacement-dependency");
    let replacement = state_from_plan(&replacement_plan, installed_from_plan(&replacement_plan));
    let mut stages = fake_stages(&replacement_plan, false);
    stages[1].fail_publication = true;
    commit_staged_graph(storage.directory(), Some(&previous), &replacement, stages)
        .expect_err("injected mid-commit failure");
    let snapshot = crate::state::load_rollback_snapshot(storage.directory())
        .expect("load rollback")
        .expect("rollback");
    crate::state::restore_rollback_snapshot(storage.directory(), &snapshot).expect("restore prior graph");

    assert_graph_files(&root, b"root", b"dependency");
    assert_eq!(
        fs::read(root.join(".axial-lock.json")).expect("lock bytes"),
        lock_before
    );
    assert_eq!(
        crate::state::load_state(storage.directory())
            .expect("load state")
            .expect("prior state"),
        previous
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn commit_collision_never_claims_or_removes_user_owned_destination() {
    let root = test_root("user-destination-collision");
    let storage = anchor(&root);
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    fs::write(root.join("root.jar"), b"user-owned").expect("write user file");

    commit_staged_graph(storage.directory(), None, &state, fake_stages(&plan, false))
        .expect_err("untracked destination must block publication");
    crate::state::reconcile_managed_addition_obligations(storage.directory(), None)
        .expect("discard untracked managed obligations");

    assert_eq!(
        fs::read(root.join("root.jar")).expect("user destination preserved"),
        b"user-owned"
    );
    assert!(!root.join("dependency.jar").exists());
    assert!(
        crate::state::load_state(storage.directory())
            .expect("load state")
            .is_none()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn full_graph_remove_and_rollback_preserve_dependencies() {
    let root = test_root("remove-rollback");
    let storage = anchor(&root);
    let plan = graph_plan(b"root", b"dependency");
    let state = state_from_plan(&plan, installed_from_plan(&plan));
    commit_staged_graph(storage.directory(), None, &state, fake_stages(&plan, false))
        .expect("install graph");

    remove_managed_transaction(storage.directory()).expect("remove graph");
    assert!(!root.join("root.jar").exists());
    assert!(!root.join("dependency.jar").exists());
    let snapshot = crate::state::load_rollback_snapshot(storage.directory())
        .expect("load rollback")
        .expect("rollback");
    assert!(matches!(
        crate::state::restore_rollback_snapshot(storage.directory(), &snapshot).expect("restore graph"),
        crate::state::ManagedRollbackOutcome::ManagedComposition(_)
    ));
    assert_graph_files(&root, b"root", b"dependency");
    let _ = fs::remove_dir_all(root);
}

fn graph_plan(root: &[u8], dependency: &[u8]) -> ManagedCompositionInstallPlan {
    graph_plan_at(root, dependency, "https://cdn.example.invalid")
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

struct TestManagedRoot {
    directory: crate::storage::ManagedStorageDirectory,
    _session: RootSession,
    _cleanup: TestAuthorityRootCleanup,
}

impl TestManagedRoot {
    fn directory(&self) -> &crate::storage::ManagedStorageDirectory {
        &self.directory
    }
}

struct TestAuthorityRootCleanup(PathBuf);

impl Drop for TestAuthorityRootCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn anchor(root: &Path) -> TestManagedRoot {
    let authority_root = root.with_extension("axial-test-authority");
    let session = acquire_test_root_session(&authority_root);
    let directory = session
        .admit_absolute_directory(root)
        .expect("admit managed test directory");
    let effects = crate::storage::ManagedInstanceEffectAuthority::bind(&directory)
        .expect("bind managed test effect authority");
    TestManagedRoot {
        directory: crate::storage::ManagedStorageDirectory::bind_instance_root(directory, effects)
            .expect("bind managed test directory"),
        _session: session,
        _cleanup: TestAuthorityRootCleanup(authority_root),
    }
}

fn acquire_test_root_session(root: &Path) -> RootSession {
    match RootSession::acquire(root) {
        RootSessionAcquireOutcome::Acquired(session) => session,
        RootSessionAcquireOutcome::NoEffect(error) => {
            panic!("test root authority acquisition had no effect: {error}")
        }
        RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            match obligation.reconcile() {
                RootSessionAcquireOutcome::Acquired(session) => session,
                RootSessionAcquireOutcome::NoEffect(error) => {
                    panic!("test root authority reconciliation had no effect: {error}")
                }
                RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                    let message = obligation.error().to_string();
                    match obligation.cleanup() {
                        Ok(()) => panic!(
                            "test root authority was cleaned after indeterminate acquisition: {message}"
                        ),
                        Err(obligation) => {
                            std::mem::forget(obligation);
                            panic!(
                                "test root authority remains retained after indeterminate acquisition: {message}"
                            );
                        }
                    }
                }
            }
        }
    }
}

fn publish_test_bytes_create_new(
    destination: &crate::storage::ManagedStorageDirectory,
    filename: &str,
    bytes: &[u8],
) -> Result<(), InstallError> {
    let mut staged = match destination.directory().create_stage() {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => return Err(InstallError::Io(error)),
        FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileCreateResolution::Created(staged) => staged,
            FileCreateResolution::Indeterminate(obligation) => {
                let error = copy_io_error(obligation.error());
                return Err(retained_test_effect(
                    error,
                    RetainedTestEffect::FileCreate(obligation),
                ));
            }
        },
    };
    if let Err(error) = staged.write_all(bytes) {
        settle_test_stage_discard(staged.discard())?;
        return Err(InstallError::Io(error));
    }
    let sealed = match staged.seal() {
        Ok(sealed) => sealed,
        Err(failure) => {
            let error = copy_io_error(failure.error());
            settle_test_stage_discard(failure.into_staged().discard())?;
            return Err(InstallError::Io(error));
        }
    };
    let filename = LeafName::new(filename).map_err(|_| {
        InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "test artifact filename is not a direct leaf",
        ))
    })?;
    match sealed.promote_no_replace(
        destination.directory(),
        destination.directory(),
        &filename,
    ) {
        FilePromotionOutcome::Applied(file) => {
            drop(file);
            Ok(())
        }
        FilePromotionOutcome::NoEffect { error, staged } => {
            settle_test_stage_discard(staged.discard())?;
            Err(InstallError::Io(error))
        }
        FilePromotionOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FilePromotionResolution::Applied(file) => {
                drop(file);
                Ok(())
            }
            FilePromotionResolution::NoEffect(staged) => {
                settle_test_stage_discard(staged.discard())?;
                Err(InstallError::Io(std::io::Error::other(
                    "test artifact publication had no effect",
                )))
            }
            FilePromotionResolution::Indeterminate(obligation) => {
                let error = copy_io_error(obligation.error());
                Err(retained_test_effect(
                    error,
                    RetainedTestEffect::FilePromotion(obligation),
                ))
            }
        },
    }
}

fn settle_test_stage_discard(outcome: StageDiscardOutcome) -> Result<(), InstallError> {
    match outcome {
        StageDiscardOutcome::Discarded => Ok(()),
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            let error = copy_io_error(obligation.error());
            match obligation.reconcile() {
                StageDiscardResolution::Discarded => Ok(()),
                StageDiscardResolution::Indeterminate(obligation) => {
                    Err(retained_test_effect(
                        error,
                        RetainedTestEffect::StageDiscard(obligation),
                    ))
                }
            }
        }
    }
}

fn copy_io_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), error.to_string())
}

enum RetainedTestEffect {
    FileCreate(FileCreateObligation),
    FilePromotion(FilePromotionObligation),
    StageDiscard(StageDiscardObligation),
}

struct RetainedTestEffectError {
    source: std::io::Error,
    _effect: RetainedTestEffect,
}

impl std::fmt::Debug for RetainedTestEffectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedTestEffectError")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RetainedTestEffectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("test filesystem effect remains retained and indeterminate")
    }
}

impl std::error::Error for RetainedTestEffectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn retained_test_effect(error: std::io::Error, effect: RetainedTestEffect) -> InstallError {
    InstallError::Io(std::io::Error::other(RetainedTestEffectError {
        source: error,
        _effect: effect,
    }))
}

fn test_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "axial-performance-install-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create test root");
    path
}
