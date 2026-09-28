use super::*;
use crate::{
    files::PortableName,
    library::{LibraryLifecycle, LibraryOpenOutcome},
    storage::MetadataStore,
};
use axial_performance::{
    CompositionPlan, CompositionTier, ManagedCompositionInstallPlan, ManagedRollbackOutcome,
    PerformanceManager, PerformanceMode,
};
use std::sync::Arc;

fn open_library(path: &std::path::Path) -> LibraryLifecycle {
    match LibraryLifecycle::open(path) {
        LibraryOpenOutcome::Ready(library) => library,
        LibraryOpenOutcome::NoEffect(error) => panic!("fixture library failed: {error}"),
        LibraryOpenOutcome::Unresolved(obligation) => {
            obligation.acknowledge_preserved().unwrap();
            panic!("fixture library was unresolved");
        }
    }
}

fn fixture_directory() -> tempfile::TempDir {
    let parent = std::env::temp_dir().canonicalize().unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn empty_plan() -> ManagedCompositionInstallPlan {
    ManagedCompositionInstallPlan::seal(
        CompositionPlan {
            composition_id: "fixture-managed".into(),
            family: axial_performance::types::VersionFamily::F,
            loader: "fabric".into(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: Vec::new(),
            jvm_preset: String::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        },
        "1.21.1",
        "fabric",
        Vec::new(),
        Vec::new(),
    )
    .unwrap()
}

#[tokio::test]
async fn capability_bound_apply_remove_and_rollback_preserve_unmanaged_files_and_generation() {
    let fixture = fixture_directory();
    let library = open_library(fixture.path());
    let pin = library.admit().unwrap();
    let path = pin.read_projection().unwrap().join("registered");
    std::fs::create_dir_all(path.join("mods")).unwrap();
    std::fs::write(path.join("mods/user.jar"), b"user owned bytes").unwrap();
    let scope = pin
        .files()
        .unwrap()
        .open_directory(&PortableName::new_exact("registered").unwrap())
        .unwrap();
    let manager = Arc::new(PerformanceManager::new().unwrap());
    let (authority, identity) = manager
        .bind_admitted_instance(
            "1c53a187-80d2-4396-9dc8-1d11dc3f03d0",
            scope.capability().clone(),
        )
        .unwrap();
    let effects = authority
        .bind_instance_effect_authority(&identity)
        .await
        .unwrap();
    let outcome = authority
        .ensure_installed(
            &identity,
            &effects,
            &empty_plan(),
            public_transfer_resolver(),
            || async { Ok::<_, ()>(()) },
        )
        .await
        .unwrap();
    assert!(outcome.target_changed());
    assert!(
        authority
            .recover_and_inspect(&identity, &effects)
            .await
            .unwrap()
            .state
            .is_some()
    );
    authority.remove_managed(&identity, &effects).await.unwrap();
    assert!(
        authority
            .recover_and_inspect(&identity, &effects)
            .await
            .unwrap()
            .state
            .is_none()
    );
    assert!(matches!(
        authority
            .rollback_managed(&identity, &effects)
            .await
            .unwrap(),
        ManagedRollbackOutcome::ManagedComposition(_)
    ));
    assert_eq!(
        std::fs::read(path.join("mods/user.jar")).unwrap(),
        b"user owned bytes"
    );
    // An escaped capability plus its generation pin blocks library revocation.
    library.close_admission();
    assert!(library.revoke_application_root().is_err());
    drop((effects, identity, authority, scope, pin));
    assert!(matches!(
        library.revoke_application_root().unwrap(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[tokio::test]
async fn admitted_identity_cannot_be_substituted_across_directory_authorities() {
    let fixture = fixture_directory();
    let library = open_library(fixture.path());
    let pin = library.admit().unwrap();
    for name in ["a", "b"] {
        std::fs::create_dir_all(pin.read_projection().unwrap().join(name)).unwrap();
    }
    let root = pin.files().unwrap();
    let a = root
        .open_directory(&PortableName::new_exact("a").unwrap())
        .unwrap();
    let b = root
        .open_directory(&PortableName::new_exact("b").unwrap())
        .unwrap();
    let manager = Arc::new(PerformanceManager::new().unwrap());
    let (authority_a, identity_a) = manager
        .bind_admitted_instance("same-label", a.capability().clone())
        .unwrap();
    let (authority_b, identity_b) = manager
        .bind_admitted_instance("same-label", b.capability().clone())
        .unwrap();
    assert!(
        authority_a
            .bind_instance_effect_authority(&identity_b)
            .await
            .is_err()
    );
    let effects_a = authority_a
        .bind_instance_effect_authority(&identity_a)
        .await
        .unwrap();
    let effects_b = authority_b
        .bind_instance_effect_authority(&identity_b)
        .await
        .unwrap();
    assert!(
        authority_a
            .remove_managed(&identity_a, &effects_b)
            .await
            .is_err()
    );
    assert!(
        authority_a
            .recover_and_inspect(&identity_a, &effects_a)
            .await
            .unwrap()
            .state
            .is_none()
    );
    drop((
        effects_a,
        effects_b,
        identity_a,
        identity_b,
        authority_a,
        authority_b,
        a,
        b,
        root,
        pin,
    ));
    library.close_admission();
    assert!(matches!(
        library.revoke_application_root().unwrap(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[tokio::test]
async fn cancelled_target_checkpoint_does_not_publish_managed_state() {
    let fixture = fixture_directory();
    let library = open_library(fixture.path());
    let pin = library.admit().unwrap();
    let scope = pin.files().unwrap();
    let manager = Arc::new(PerformanceManager::new().unwrap());
    let (authority, identity) = manager
        .bind_admitted_instance("registered", scope.capability().clone())
        .unwrap();
    let effects = authority
        .bind_instance_effect_authority(&identity)
        .await
        .unwrap();
    assert!(
        authority
            .ensure_installed(
                &identity,
                &effects,
                &empty_plan(),
                public_transfer_resolver(),
                || async { Err::<(), _>("cancelled") }
            )
            .await
            .is_err()
    );
    let recovered = authority
        .recover_and_inspect(&identity, &effects)
        .await
        .unwrap();
    assert!(recovered.state.is_none());
    assert!(
        recovered
            .rollback_snapshots
            .iter()
            .any(|snapshot| snapshot.rollback_available)
    );
    drop((effects, identity, authority, scope, pin));
    library.close_admission();
    assert!(matches!(
        library.revoke_application_root().unwrap(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[tokio::test]
async fn active_rule_lease_prevents_refresh_from_replacing_a_prepared_plan() {
    let storage = Arc::new(MetadataStore::in_memory().unwrap());
    storage.migrate(&[rules::MIGRATION]).unwrap();
    let rules = rules::PerformanceRules::with_remote(storage, None, None).unwrap();
    let planned = rules
        .plan(axial_performance::ResolutionRequest {
            game_version: "1.21.1".into(),
            loader: "fabric".into(),
            mode: PerformanceMode::Managed,
            hardware: Default::default(),
            installed_mods: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), rules.refresh())
            .await
            .is_err()
    );
    planned.ensure_current().unwrap();
    drop(planned);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rules.refresh())
            .await
            .is_ok()
    );
}
