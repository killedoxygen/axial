use super::*;
use sha2::{Digest, Sha256};

fn acquire_test_root(path: &Path) -> RootSession {
    match RootSession::acquire(path) {
        RootSessionAcquireOutcome::Acquired(session) => session,
        RootSessionAcquireOutcome::NoEffect(error) => {
            panic!("root acquisition had no effect: {error}")
        }
        RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            let initial_error = format!("{:?}", obligation.error());
            match obligation.reconcile() {
                RootSessionAcquireOutcome::Acquired(session) => session,
                RootSessionAcquireOutcome::NoEffect(error) => {
                    panic!("root acquisition reconciliation had no effect: {error}")
                }
                RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                    let error = format!("{:?}", obligation.error());
                    let obligation = obligation
                        .cleanup()
                        .expect_err("acquired recovery owner refuses test cleanup");
                    obligation
                        .acknowledge_preserved()
                        .expect("acknowledge failed test acquisition");
                    panic!(
                        "root acquisition remained indeterminate: initial {initial_error}; retry {error}"
                    )
                }
            }
        }
    }
}

#[test]
fn root_session_creates_a_missing_nested_root() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let root_path = temporary.path().join("missing").join("nested-root");

    let session = acquire_test_root(&root_path);

    assert!(root_path.is_dir());
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn cloned_root_capabilities_serialize_multipage_enumeration() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let expected = (0..384)
        .map(|index| format!("entry-{index:03}-{}", "x".repeat(180)))
        .collect::<std::collections::BTreeSet<_>>();
    for name in &expected {
        std::fs::write(temporary.path().join(name), b"").expect("directory entry");
    }
    let session = acquire_test_root(temporary.path());
    let first = session.root().expect("first root capability");
    let second = session.root().expect("second root capability");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let enumerate = |root: Directory, barrier: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            let mut observed = None;
            for _ in 0..8 {
                let listing = root.entries(512).expect("complete directory listing");
                assert_eq!(listing.state(), DirectoryListingState::Complete);
                let names = listing
                    .entries()
                    .iter()
                    .filter_map(DirectoryEntry::utf8_name)
                    .map(str::to_owned)
                    .collect::<std::collections::BTreeSet<_>>();
                if let Some(previous) = &observed {
                    assert_eq!(previous, &names);
                } else {
                    observed = Some(names);
                }
            }
            observed.expect("enumerated names")
        })
    };
    let first = enumerate(first, std::sync::Arc::clone(&barrier));
    let second = enumerate(second, barrier);
    let first = first.join().expect("first enumeration worker");
    let second = second.join().expect("second enumeration worker");

    assert_eq!(first, second);
    assert!(expected.is_subset(&first));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn absolute_projection_tolerates_one_harmless_sibling_change() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let root_path = temporary.path().join("projection-root");
    std::fs::create_dir(&root_path).expect("projection root");
    let session = acquire_test_root(&root_path);
    let root = session.root().expect("root capability");
    let target_name = root_path
        .file_name()
        .expect("projection root name")
        .to_os_string();
    let sibling = temporary.path().join("harmless-sibling");
    let mutations = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let hook_mutations = std::rc::Rc::clone(&mutations);
    let hook = platform::install_exact_directory_binding_test_hook(Box::new(move |name| {
        if name == target_name && hook_mutations.get() == 0 {
            std::fs::create_dir(&sibling).expect("create harmless sibling");
            hook_mutations.set(1);
        }
    }));

    root.validate_absolute_projection(&root_path)
        .expect("one sibling change does not invalidate the target");

    assert_eq!(mutations.get(), 1);
    drop(hook);
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn absolute_projection_tolerates_sustained_sibling_churn() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let root_path = temporary.path().join("projection-root");
    std::fs::create_dir(&root_path).expect("projection root");
    let session = acquire_test_root(&root_path);
    let root = session.root().expect("root capability");
    let target_name = root_path
        .file_name()
        .expect("projection root name")
        .to_os_string();
    let sibling_parent = temporary.path().to_path_buf();
    let mutations = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let hook_mutations = std::rc::Rc::clone(&mutations);
    let hook = platform::install_exact_directory_binding_test_hook(Box::new(move |name| {
        if name != target_name {
            return;
        }
        for mutation in 0..64 {
            let sibling = sibling_parent.join(format!("unstable-sibling-{mutation}"));
            std::fs::create_dir(&sibling).expect("create unstable sibling");
            std::fs::remove_dir(&sibling).expect("remove unstable sibling");
            hook_mutations.set(hook_mutations.get() + 1);
        }
    }));

    root.validate_absolute_projection(&root_path)
        .expect("sustained sibling churn does not invalidate the target");

    assert_eq!(mutations.get(), 64);
    drop(hook);
    root.validate_absolute_projection(&root_path)
        .expect("unchanged target remains valid after churn");
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn absolute_projection_refuses_case_alias_introduced_during_proof() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let root_path = temporary.path().join("projection-root");
    let alias_path = temporary.path().join("Projection-Root");
    std::fs::create_dir(&root_path).expect("projection root");
    let session = acquire_test_root(&root_path);
    let root = session.root().expect("root capability");
    let target_name = root_path
        .file_name()
        .expect("projection root name")
        .to_os_string();
    let hook_root = root_path.clone();
    let hook_alias = alias_path.clone();
    let renamed = std::rc::Rc::new(std::cell::Cell::new(false));
    let hook_renamed = std::rc::Rc::clone(&renamed);
    let hook = platform::install_exact_directory_binding_test_hook(Box::new(move |name| {
        if name == target_name && !hook_renamed.replace(true) {
            std::fs::rename(&hook_root, &hook_alias).expect("introduce case alias");
        }
    }));

    let error = root
        .validate_absolute_projection(&root_path)
        .expect_err("case alias must invalidate exact projection");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(renamed.get());
    drop(hook);
    std::fs::rename(&alias_path, &root_path).expect("restore exact root spelling");
    root.validate_absolute_projection(&root_path)
        .expect("restored exact target remains valid");
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn owned_symlink_proves_a_parent_relative_target_beneath_its_root() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("bin")).expect("bin directory");
    std::fs::create_dir(temporary.path().join("lib")).expect("lib directory");
    std::fs::write(temporary.path().join("lib/runtime.bin"), b"runtime").expect("runtime target");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let bin = LeafName::new("bin").expect("bin leaf");
    let link = LeafName::new("runtime").expect("link leaf");

    root.create_owned_symlink_beneath(
        std::slice::from_ref(&bin),
        &link,
        OsStr::new("../lib/runtime.bin"),
    )
    .expect("contained owned link");

    let bin_directory = root.open_directory(&bin).expect("bin capability");
    assert_eq!(
        bin_directory.read_symlink(&link).expect("link target"),
        OsStr::new("../lib/runtime.bin")
    );
    drop((bin_directory, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn owned_symlink_rejects_escape_missing_and_link_targets() {
    use std::os::unix::fs::symlink;

    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("bin")).expect("bin directory");
    std::fs::create_dir(temporary.path().join("lib")).expect("lib directory");
    std::fs::write(temporary.path().join("lib/runtime.bin"), b"runtime").expect("runtime target");
    symlink("runtime.bin", temporary.path().join("lib/runtime.alias"))
        .expect("target link fixture");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let bin = LeafName::new("bin").expect("bin leaf");

    for (name, target, kind) in [
        ("escape", "../../outside", io::ErrorKind::InvalidInput),
        ("missing", "../lib/missing", io::ErrorKind::NotFound),
        (
            "link-target",
            "../lib/runtime.alias",
            io::ErrorKind::InvalidInput,
        ),
    ] {
        assert_eq!(
            root.create_owned_symlink_beneath(
                std::slice::from_ref(&bin),
                &LeafName::new(name).expect("link leaf"),
                OsStr::new(target),
            )
            .expect_err("unsafe target must be rejected")
            .kind(),
            kind
        );
        assert!(!temporary.path().join("bin").join(name).exists());
    }
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn admitted_absolute_directory_retains_one_physical_binding() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let session = acquire_test_root(&app_root);
    {
        let admitted = session
            .admit_absolute_directory_authority(&library)
            .expect("admit library");
        assert_eq!(
            admitted.filesystem_identity().expect("admitted identity"),
            session
                .admit_absolute_directory(&library)
                .expect("repeat admission")
                .identity()
                .expect("repeat identity")
                .filesystem_identity()
        );
        admitted.revalidate().expect("revalidate admission");
    }
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn admitted_absolute_directory_supports_staged_file_effects() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let session = acquire_test_root(&app_root);
    let admitted = session
        .admit_absolute_directory(&library)
        .expect("admit library");

    let staged = test_sealed_stage(&admitted, b"managed payload");
    let name = LeafName::new("published.bin").expect("published leaf");
    let published = require_test_promotion(staged.promote_no_replace(&admitted, &admitted, &name));

    drop(published);
    assert_eq!(
        std::fs::read(library.join("published.bin")).expect("published payload"),
        b"managed payload"
    );
    drop(admitted);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn scratch_sealed_stage_admits_durable_read_capability_at_promotion() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let mut staged = match root.create_stage() {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => panic!("stage creation failed: {error}"),
        FileCreateOutcome::AppliedUnverified(obligation) => {
            panic!("stage creation was not verified: {}", obligation.error())
        }
    };
    staged.write_all(b"scratch payload").expect("stage bytes");
    let sealed = staged.seal_scratch().expect("scratch seal");
    assert!(sealed.published.is_none());

    let name = LeafName::new("published.bin").expect("published leaf");
    let published = require_test_promotion(sealed.promote_no_replace(&root, &root, &name));
    assert_eq!(
        published
            .read_bounded(64)
            .expect("promoted capability reads"),
        b"scratch payload"
    );
    drop(published);
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn sealed_stage_promotes_across_directories() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("source")).expect("source directory");
    std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let source = root
        .open_directory(&LeafName::new("source").expect("source leaf"))
        .expect("source capability");
    let destination = root
        .open_directory(&LeafName::new("destination").expect("destination leaf"))
        .expect("destination capability");

    let staged = test_sealed_stage(&source, b"managed payload");
    let name = LeafName::new("published.bin").expect("published leaf");
    let published = require_test_promotion(staged.promote_no_replace(&source, &destination, &name));

    assert_eq!(
        published.read_bounded(32).expect("published payload"),
        b"managed payload"
    );
    drop((root, source, destination, published));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn sealed_stage_collision_is_no_effect() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("published.bin"), b"existing payload")
        .expect("collision destination");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let staged = test_sealed_stage(&root, b"new payload");
    let name = LeafName::new("published.bin").expect("published leaf");

    let staged = match staged.promote_no_replace(&root, &root, &name) {
        FilePromotionOutcome::NoEffect { item: staged, .. } => staged,
        FilePromotionOutcome::Applied(_) => panic!("collision replaced its destination"),
        FilePromotionOutcome::AppliedUnverified(obligation) => {
            panic!("collision remained indeterminate: {}", obligation.error())
        }
    };

    assert_eq!(
        std::fs::read(temporary.path().join("published.bin")).expect("collision payload"),
        b"existing payload"
    );
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn sealed_stage_refuses_content_revision_drift() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let staged = test_sealed_stage(&root, b"sealed payload");
    staged
        .file
        .handle
        .set_len(1)
        .expect("mutate sealed stage length");
    let name = LeafName::new("published.bin").expect("published leaf");

    let staged = match staged.promote_no_replace(&root, &root, &name) {
        FilePromotionOutcome::NoEffect { item: staged, .. } => staged,
        FilePromotionOutcome::Applied(_) => panic!("changed sealed stage was published"),
        FilePromotionOutcome::AppliedUnverified(obligation) => panic!(
            "changed sealed stage became indeterminate: {}",
            obligation.error()
        ),
    };

    assert!(!temporary.path().join("published.bin").exists());
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn sealed_stage_refuses_live_park_original_reservation() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let original_path = temporary.path().join("reserved.bin");
    std::fs::write(&original_path, b"original payload").expect("original file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_file(&root, "reserved.bin", "reserved.park", b"original payload");
    assert!(!original_path.exists());
    let staged = test_sealed_stage(&root, b"replacement payload");
    let destination = LeafName::new("RESERVED.BIN").expect("portable alias");

    let staged = match staged.promote_no_replace(&root, &root, &destination) {
        FilePromotionOutcome::NoEffect {
            error,
            item: staged,
        } => {
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            staged
        }
        FilePromotionOutcome::Applied(_) => panic!("stage occupied a live park reservation"),
        FilePromotionOutcome::AppliedUnverified(obligation) => panic!(
            "reserved stage promotion became indeterminate: {}",
            obligation.error()
        ),
    };
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    let restored = match parked.restore() {
        FileRestoreOutcome::Applied(file) => file,
        FileRestoreOutcome::NoEffect { error, .. } => {
            panic!("park restoration had no effect: {error}")
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => {
            panic!("park restoration was indeterminate: {}", obligation.error())
        }
    };
    assert_eq!(
        std::fs::read(&original_path).expect("restored payload"),
        b"original payload"
    );

    drop((restored, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_park_checkout_roundtrip_preserves_learned_revision() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("record file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_file(&root, "record.bin", "record.park", b"payload");

    parked
        .validate_current()
        .expect("first checkout retained learned revision");
    parked
        .validate_current()
        .expect("second checkout restored learned revision");
    let restored = match parked.restore() {
        FileRestoreOutcome::Applied(file) => file,
        FileRestoreOutcome::NoEffect { error, .. } => {
            panic!("park restoration had no effect: {error}")
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => {
            panic!("park restoration was indeterminate: {}", obligation.error())
        }
    };

    drop((restored, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn hardlink_aliases_cannot_hold_independent_file_parks() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("first.bin"), b"payload").expect("first file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let first = park_test_file(&root, "first.bin", "first.park", b"payload");
    std::fs::hard_link(
        temporary.path().join("first.park"),
        temporary.path().join("second.bin"),
    )
    .expect("second hardlink");
    std::fs::remove_file(temporary.path().join("first.park"))
        .expect("leave the retained inode at one disjoint link");
    let second_file = root
        .open_file(&LeafName::new("second.bin").expect("second leaf"))
        .expect("second capability");
    let second_revision = second_file.revision().expect("second revision");
    let digest: [u8; 32] = Sha256::digest(b"payload").into();
    let request = second_file.park_request(ExpectedFileContent::new(second_revision, digest));

    let second_file = match root.park_file_as(
        request,
        LeafName::new("second.park").expect("second park leaf"),
    ) {
        FileParkOutcome::NoEffect { error, request } => {
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            request.file
        }
        FileParkOutcome::Parked(_) => panic!("hardlink alias acquired a second park owner"),
        FileParkOutcome::Preserved { error, .. } => {
            panic!("hardlink conflict changed content: {error}")
        }
        FileParkOutcome::AppliedUnverified(obligation) => {
            panic!(
                "hardlink conflict became indeterminate: {}",
                obligation.error()
            )
        }
    };
    drop(second_file);
    std::fs::rename(
        temporary.path().join("second.bin"),
        temporary.path().join("first.park"),
    )
    .expect("restore first park topology");
    let restored = match first.restore() {
        FileRestoreOutcome::Applied(file) => file,
        FileRestoreOutcome::NoEffect { error, .. } => {
            panic!("first park restoration had no effect: {error}")
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => {
            panic!(
                "first park restoration was indeterminate: {}",
                obligation.error()
            )
        }
    };

    drop((restored, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn unsettled_file_move_blocks_hardlink_alias_park_ownership() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("source.bin"), b"payload").expect("source file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let source = root
        .open_file(&LeafName::new("source.bin").expect("source leaf"))
        .expect("source capability");
    let authority = root.authority().expect("root authority");
    let operation = authority.enter().expect("move reservation operation");
    let mut movement = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: source.name.clone(),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("destination.bin").expect("destination leaf"),
        },
        None,
        Some(source.identity),
    )
    .expect("file move reservation");
    let rebind = std::fs::hard_link(
        temporary.path().join("source.bin"),
        temporary.path().join("alias.bin"),
    )
    .and_then(|()| std::fs::remove_file(temporary.path().join("source.bin")));
    if let Err(error) = rebind {
        movement
            .settle(&operation)
            .expect("failed-rebind move settlement");
        panic!("hardlink single-link rebind failed: {error}");
    }
    let alias = match root.open_file(&LeafName::new("alias.bin").expect("alias leaf")) {
        Ok(alias) => alias,
        Err(error) => {
            movement
                .settle(&operation)
                .expect("failed-admission move settlement");
            panic!("single-link alias admission failed: {error}");
        }
    };
    let revision = match alias.revision() {
        Ok(revision) => revision,
        Err(error) => {
            drop(alias);
            movement
                .settle(&operation)
                .expect("failed-revision move settlement");
            panic!("single-link alias revision failed: {error}");
        }
    };
    let digest: [u8; 32] = Sha256::digest(b"payload").into();
    let request = alias.park_request(ExpectedFileContent::new(revision, digest));

    let outcome = root.park_file_as(
        request,
        LeafName::new("alias.park").expect("alias park leaf"),
    );
    let error = match outcome {
        FileParkOutcome::NoEffect { error, request } => {
            drop(request.file);
            error
        }
        unexpected => {
            movement
                .settle(&operation)
                .expect("unexpected-outcome move settlement");
            drop(operation);
            panic!("move-owned inode acquired a second park outcome: {unexpected:?}");
        }
    };
    if let Err(rebind) = std::fs::rename(
        temporary.path().join("alias.bin"),
        temporary.path().join("source.bin"),
    ) {
        movement
            .settle(&operation)
            .expect("failed-restore move settlement");
        panic!("single-link source restoration failed: {rebind}");
    }
    movement
        .settle(&operation)
        .expect("move reservation settlement");
    drop(operation);
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

    drop((source, authority, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replacement_handoff_publishes_while_unrelated_park_stays_reserved() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
    std::fs::write(temporary.path().join("unrelated.bin"), b"unrelated payload")
        .expect("unrelated file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let unrelated = park_test_file(
        &root,
        "unrelated.bin",
        "unrelated.park",
        b"unrelated payload",
    );
    let target = root
        .open_file(&LeafName::new("target.bin").expect("target leaf"))
        .expect("target capability");
    let revision = target.revision().expect("target revision");
    let digest: [u8; 32] = Sha256::digest(b"old payload").into();
    let request = target.park_request(ExpectedFileContent::new(revision, digest));
    let staged = test_sealed_stage(&root, b"new payload");

    let (current, displaced) =
        match staged.replace_nondurable(ReplaceDestination::Existing(request)) {
            FileReplaceOutcome::Replaced {
                current,
                displaced: Some(displaced),
            } => (current, displaced),
            FileReplaceOutcome::Replaced {
                displaced: None, ..
            } => {
                panic!("replacement lost its displaced file")
            }
            FileReplaceOutcome::NoEffect { error, .. } => {
                panic!("replacement had no effect: {error}")
            }
            FileReplaceOutcome::AppliedUnverified(obligation) => {
                panic!("replacement remained indeterminate: {}", obligation.error())
            }
        };
    assert_eq!(
        std::fs::read(temporary.path().join("target.bin")).expect("new target payload"),
        b"new payload"
    );
    displaced
        .validate_current()
        .expect("stage disarm released only the replacement handoff");
    assert!(matches!(
        displaced.remove(),
        FileRemovalOutcome::Applied(())
    ));
    let restored = match unrelated.restore() {
        FileRestoreOutcome::Applied(file) => file,
        FileRestoreOutcome::NoEffect { error, .. } => {
            panic!("unrelated park restoration had no effect: {error}")
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => panic!(
            "unrelated park restoration remained indeterminate: {}",
            obligation.error()
        ),
    };

    drop((current, restored, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn replacement_handoff_link_survives_indeterminate_cleanup_until_no_effect() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let mut parked = park_test_file(&root, "target.bin", "target.park", b"old payload");
    let mut staged = test_sealed_stage(&root, b"new payload");
    let source_name = staged.file.name.clone();
    let destination_name = LeafName::new("target.bin").expect("target leaf");
    let attempt_id = staged
        .token
        .allocate_publication_attempt()
        .expect("publication attempt identity");
    let attempt = platform::prepare_publication(
        attempt_id,
        &staged.file.handle,
        staged.revision.size,
        staged.revision.stamp,
        root.inner.handle(),
        source_name.as_os_str(),
        root.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("prepare replacement publication");
    staged
        .token
        .prepare_promotion(
            &root,
            &destination_name,
            attempt_id,
            attempt,
            Some(&parked.token),
            None,
        )
        .expect("link replacement handoff");

    assert_eq!(
        parked
            .validate_current()
            .expect_err("linked park checkout")
            .kind(),
        io::ErrorKind::WouldBlock,
    );
    let authority = parked.authority().expect("park authority");
    let operation = authority.enter().expect("park rollback operation");
    assert_eq!(
        authority
            .rollback_file_park_registration(&operation, &mut parked.token)
            .expect_err("linked park rollback")
            .kind(),
        io::ErrorKind::WouldBlock,
    );
    drop(operation);

    staged
        .token
        .update(StageRegistryPhase::Unresolved)
        .expect("mark indeterminate publication");
    assert_eq!(
        parked
            .validate_current()
            .expect_err("indeterminate link checkout")
            .kind(),
        io::ErrorKind::WouldBlock,
    );

    let staged_path = temporary.path().join(source_name.as_os_str());
    let displaced_stage_path = temporary.path().join("displaced-stage.bin");
    std::fs::rename(&staged_path, &displaced_stage_path).expect("displace stage binding");
    staged
        .token
        .discard()
        .expect_err("indeterminate cleanup must fail closed");
    assert_eq!(
        parked
            .validate_current()
            .expect_err("failed cleanup link checkout")
            .kind(),
        io::ErrorKind::WouldBlock,
    );

    std::fs::rename(&displaced_stage_path, &staged_path).expect("restore stage binding");
    staged
        .token
        .update(StageRegistryPhase::Sealed)
        .expect("classify publication as no effect");
    parked
        .validate_current()
        .expect("no-effect classification released park link");
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    let restored = match parked.restore() {
        FileRestoreOutcome::Applied(file) => file,
        FileRestoreOutcome::NoEffect { error, .. } => {
            panic!("park restoration had no effect: {error}")
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => {
            panic!("park restoration was indeterminate: {}", obligation.error())
        }
    };

    drop((restored, authority, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn dropped_replacement_obligation_settles_stage_before_linked_park() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_file(&root, "target.bin", "target.park", b"old payload");
    let staged = test_sealed_stage(&root, b"new payload");
    let staged_name = staged.file.name.clone();
    let destination_name = LeafName::new("target.bin").expect("target leaf");
    let attempt_id = staged
        .token
        .allocate_publication_attempt()
        .expect("publication attempt identity");
    let attempt = platform::prepare_publication(
        attempt_id,
        &staged.file.handle,
        staged.revision.size,
        staged.revision.stamp,
        root.inner.handle(),
        staged_name.as_os_str(),
        root.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("prepare replacement publication");
    staged
        .token
        .prepare_promotion(
            &root,
            &destination_name,
            attempt_id,
            attempt.clone(),
            Some(&parked.token),
            None,
        )
        .expect("link replacement handoff");
    let obligation = FileReplaceObligation {
        error: io::Error::other("test replacement remains unresolved"),
        state: Some(Box::new(FileReplaceObligationState::Promoting {
            promotion: Box::new(FilePromotionObligation {
                error: io::Error::other("test publication remains unresolved"),
                retained: staged,
                destination: root.clone(),
                destination_name: destination_name.clone(),
                attempt_id,
                receipt: attempt,
            }),
            displaced: Some(parked),
            fallback: ReplaceDestination::Vacant {
                parent: root.clone(),
                name: destination_name,
            },
            receipt: None,
        })),
    };

    drop(obligation);
    drop(root);
    let recovery = match session.revoke() {
        RootRevokeOutcome::Recovery { recovery } => recovery,
        outcome => panic!("dropped replacement did not expose park recovery: {outcome:?}"),
    };
    let file_count = recovery.file_count();
    let directory_count = recovery.directory_count();
    let settlement = recovery.restore_all();
    assert_eq!(file_count, 1, "only the linked park remains recoverable");
    assert_eq!(directory_count, 0);
    assert!(matches!(settlement, RootRevokeOutcome::Revoked));
    assert_eq!(
        std::fs::read(temporary.path().join("target.bin")).expect("restored target"),
        b"old payload",
    );
    assert!(!temporary.path().join(staged_name.as_os_str()).exists());
}

#[test]
fn absolute_descendant_admission_cannot_bypass_directory_park_reservation() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("tree")).expect("test tree");
    std::fs::create_dir(temporary.path().join("tree/child")).expect("test child");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_directory(&root, "tree", "tree.park");
    let admitted = session
        .admit_absolute_directory(&temporary.path().join("tree.park/child"))
        .expect("absolute parked descendant");
    let authority = admitted.authority().expect("descendant authority");
    let operation = authority.enter().expect("descendant operation");
    let stage_name = LeafName::new("nested-stage.bin").expect("nested stage leaf");
    let error = authority
        .reserve_stage_create(&operation, &admitted, &stage_name, None)
        .expect_err("absolute admission bypassed directory park reservation");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    drop((operation, authority, admitted));

    let restored = match parked.restore() {
        DirectoryRestoreOutcome::Applied(directory) => directory,
        DirectoryRestoreOutcome::NoEffect { error, .. } => {
            panic!("directory restoration had no effect: {error}")
        }
        DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
            panic!(
                "directory restoration was indeterminate: {}",
                obligation.error()
            )
        }
    };
    drop((restored, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_subtree_reservations_reject_nested_owners_in_both_orders() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("tree")).expect("test tree");
    std::fs::create_dir(temporary.path().join("tree/child")).expect("test child");
    std::fs::write(temporary.path().join("tree/child/owned.bin"), b"payload").expect("nested file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let tree = root
        .open_directory(&LeafName::new("tree").expect("tree leaf"))
        .expect("tree capability");
    let child = tree
        .open_directory(&LeafName::new("child").expect("child leaf"))
        .expect("child capability");
    let nested_park = park_test_file(&child, "owned.bin", "owned.park", b"payload");

    let tree = match tree.park_as(LeafName::new("tree.park").expect("tree park leaf")) {
        DirectoryParkOutcome::NoEffect {
            error,
            item: directory,
        } => {
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            directory
        }
        DirectoryParkOutcome::Applied(_) => {
            panic!("directory park ignored a nested file park owner")
        }
        DirectoryParkOutcome::AppliedUnverified(obligation) => panic!(
            "directory park conflict became indeterminate: {}",
            obligation.error()
        ),
    };
    let nested_file = match nested_park.restore() {
        FileRestoreOutcome::Applied(file) => file,
        FileRestoreOutcome::NoEffect { error, .. } => {
            panic!("nested file restoration had no effect: {error}")
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => {
            panic!(
                "nested file restoration was indeterminate: {}",
                obligation.error()
            )
        }
    };
    drop((nested_file, child));

    let parked_tree = match tree.park_as(LeafName::new("tree.park").expect("tree park leaf")) {
        DirectoryParkOutcome::Applied(parked) => parked,
        DirectoryParkOutcome::NoEffect { error, .. } => {
            panic!("tree park had no effect: {error}")
        }
        DirectoryParkOutcome::AppliedUnverified(obligation) => {
            panic!("tree park was indeterminate: {}", obligation.error())
        }
    };
    let parked_tree_view = root
        .open_directory(&LeafName::new("tree.park").expect("parked tree leaf"))
        .expect("parked tree capability");
    let parked_child = parked_tree_view
        .open_directory(&LeafName::new("child").expect("parked child leaf"))
        .expect("parked child capability");
    let authority = root.authority().expect("root authority");
    let operation = authority.enter().expect("nested reservation operation");
    assert_eq!(
        MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: parked_child.clone(),
                name: LeafName::new("move-source").expect("move source"),
            },
            NamespaceLeaf {
                parent: parked_child.clone(),
                name: LeafName::new("move-destination").expect("move destination"),
            },
            None,
            None,
        )
        .expect_err("directory park must block a nested move")
        .kind(),
        io::ErrorKind::WouldBlock,
    );
    assert_eq!(
        authority
            .reserve_stage_create(
                &operation,
                &parked_child,
                &LeafName::new("nested-stage").expect("nested stage"),
                None,
            )
            .expect_err("directory park must block a nested stage create")
            .kind(),
        io::ErrorKind::AlreadyExists,
    );
    assert_eq!(
        authority
            .reserve_directory_create(
                &operation,
                &parked_child,
                &LeafName::new("nested-directory").expect("nested directory"),
            )
            .expect_err("directory park must block a nested directory create")
            .kind(),
        io::ErrorKind::AlreadyExists,
    );
    drop(operation);
    let staged = test_sealed_stage(&root, b"nested publication");
    let staged = match staged.promote_no_replace(
        &root,
        &parked_child,
        &LeafName::new("nested-publication").expect("nested publication"),
    ) {
        FilePromotionOutcome::NoEffect {
            error,
            item: staged,
        } => {
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            staged
        }
        FilePromotionOutcome::Applied(_) => {
            panic!("directory park allowed a nested stage promotion")
        }
        FilePromotionOutcome::AppliedUnverified(obligation) => panic!(
            "nested stage promotion conflict became indeterminate: {}",
            obligation.error()
        ),
    };
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    let nested_file = parked_child
        .open_file(&LeafName::new("owned.bin").expect("nested file leaf"))
        .expect("nested file capability");
    let revision = nested_file.revision().expect("nested revision");
    let digest: [u8; 32] = Sha256::digest(b"payload").into();
    let request = nested_file.park_request(ExpectedFileContent::new(revision, digest));
    let nested_file = match parked_child.park_file_as(
        request,
        LeafName::new("owned.second-park").expect("second nested park"),
    ) {
        FileParkOutcome::NoEffect { error, request } => {
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            request.file
        }
        FileParkOutcome::Parked(_) => {
            panic!("directory park ignored a nested file park registration")
        }
        FileParkOutcome::Preserved { error, .. } => {
            panic!("directory park conflict changed nested content: {error}")
        }
        FileParkOutcome::AppliedUnverified(obligation) => panic!(
            "nested file park conflict became indeterminate: {}",
            obligation.error()
        ),
    };
    drop((nested_file, parked_child, parked_tree_view, authority));
    let restored_tree = match parked_tree.restore() {
        DirectoryRestoreOutcome::Applied(directory) => directory,
        DirectoryRestoreOutcome::NoEffect { error, .. } => {
            panic!("tree restoration had no effect: {error}")
        }
        DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
            panic!("tree restoration was indeterminate: {}", obligation.error())
        }
    };

    drop((restored_tree, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_open_refuses_live_create_before_identity_attachment() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let authority = root.authority().expect("root authority");
    let operation = authority.enter().expect("directory create operation");
    let name = LeafName::new("created").expect("created leaf");
    let mut token = authority
        .reserve_directory_create(&operation, &root, &name)
        .expect("directory create reservation");
    let created = match platform::create_directory(root.inner.handle(), name.as_os_str()) {
        Ok(created) => created,
        Err(_) => panic!("native directory creation failed"),
    };

    let error = root
        .open_directory(&LeafName::new("CREATED").expect("portable alias"))
        .expect_err("live create reservation must block directory admission");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

    authority.attach_directory_create(&token, created);
    let directory = finish_directory_create(&authority, &operation, &mut token)
        .expect("finish directory creation");
    drop((directory, operation, authority, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_open_rechecks_create_reservation_after_native_open() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("created")).expect("initial directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let authority = root.authority().expect("root authority");
    let name = LeafName::new("created").expect("created leaf");
    let prechecked = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    *authority
        .directory_open_pause
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(DirectoryOpenReservationPause {
        parent: root.inner.identity,
        name: name.clone(),
        prechecked: Arc::clone(&prechecked),
        resume: Arc::clone(&resume),
    });
    let opening = {
        let root = root.clone();
        let name = name.clone();
        std::thread::spawn(move || root.open_directory(&name))
    };
    prechecked.wait();

    std::fs::remove_dir(temporary.path().join("created")).expect("remove initial directory");
    let operation = authority.enter().expect("directory create operation");
    let mut token = authority
        .reserve_directory_create(&operation, &root, &name)
        .expect("directory create reservation");
    let created = match platform::create_directory(root.inner.handle(), name.as_os_str()) {
        Ok(created) => created,
        Err(_) => panic!("native directory creation failed"),
    };
    authority.attach_directory_create(&token, created);
    resume.wait();

    let error = opening
        .join()
        .expect("directory open thread")
        .expect_err("post-open reservation recheck must reject admission");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    *authority
        .directory_open_pause
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
    let directory = finish_directory_create(&authority, &operation, &mut token)
        .expect("finish directory creation");
    drop((directory, operation, authority, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn identity_pending_directory_create_allows_source_stages_in_ancestors() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("runtime")).expect("runtime parent");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let runtime = root
        .open_directory(&LeafName::new("runtime").expect("runtime leaf"))
        .expect("runtime capability");
    let destinations = [&root, &runtime]
        .into_iter()
        .map(|directory| {
            directory
                .admit_transient_destination(LeafName::new("source").expect("source leaf"))
                .expect("admit source before runtime directory creation")
        })
        .collect::<Vec<_>>();
    let authority = root.authority().expect("root authority");
    let operation = authority.enter().expect("directory create operation");
    let name = LeafName::new("bin").expect("created leaf");
    let mut token = authority
        .reserve_directory_create(&operation, &runtime, &name)
        .expect("directory create reservation");
    let created = match platform::create_directory(runtime.inner.handle(), name.as_os_str()) {
        Ok(created) => created,
        Err(_) => panic!("native directory creation failed"),
    };

    let stages = destinations
        .into_iter()
        .map(TransientDestination::create_stage)
        .collect::<Vec<_>>();
    authority.attach_directory_create(&token, created);
    let directory = finish_directory_create(&authority, &operation, &mut token)
        .expect("finish directory creation");
    let mut failures = Vec::new();
    for outcome in stages {
        let destination = match outcome {
            TransientStageCreateOutcome::Created(stage) => match stage.discard() {
                TransientDiscardOutcome::Discarded(destination) => destination,
                outcome => panic!("source stage cleanup failed: {outcome:?}"),
            },
            TransientStageCreateOutcome::NoEffect { error, destination } => {
                failures.push(error);
                destination
            }
            outcome => panic!("source stage creation remained pending: {outcome:?}"),
        };
        assert!(matches!(
            destination.cancel(),
            TransientDestinationCancelOutcome::Cancelled
        ));
    }
    drop((directory, operation, authority, runtime, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(
        failures.is_empty(),
        "a new child cannot contain source stages in its parent or ancestors: {failures:?}"
    );
}

#[test]
fn identity_pending_directory_create_blocks_nested_reservations() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let authority = root.authority().expect("root authority");
    let name = LeafName::new("created").expect("created leaf");
    let operation = authority.enter().expect("directory create operation");
    let mut token = authority
        .reserve_directory_create(&operation, &root, &name)
        .expect("directory create reservation");
    let created = match platform::create_directory(root.inner.handle(), name.as_os_str()) {
        Ok(created) => created,
        Err(_) => panic!("native directory creation failed"),
    };
    let admitted = session
        .admit_absolute_directory(&temporary.path().join("created"))
        .expect("admit identity-pending directory");
    let nested_operation = authority.enter().expect("nested reservation operation");
    let nested_name = LeafName::new("nested-stage").expect("nested stage leaf");
    assert_eq!(
        authority
            .reserve_stage_create(&nested_operation, &admitted, &nested_name, None)
            .expect_err("identity-pending create must block nested stage")
            .kind(),
        io::ErrorKind::AlreadyExists,
    );
    assert_eq!(
        MoveEffectToken::reserve(
            &authority,
            &nested_operation,
            NamespaceLeaf {
                parent: admitted.clone(),
                name: LeafName::new("move-source").expect("move source"),
            },
            NamespaceLeaf {
                parent: admitted.clone(),
                name: LeafName::new("move-destination").expect("move destination"),
            },
            None,
            None,
        )
        .expect_err("identity-pending create must block nested move")
        .kind(),
        io::ErrorKind::WouldBlock,
    );
    drop(nested_operation);

    authority.attach_directory_create(&token, created);
    let directory = finish_directory_create(&authority, &operation, &mut token)
        .expect("finish directory creation");
    drop(operation);
    let operation = authority.enter().expect("settled nested reservation");
    let mut stage = authority
        .reserve_stage_create(&operation, &directory, &nested_name, None)
        .expect("settled create permits nested stage");
    let guard = authority
        .take_stage_create(&operation, &stage)
        .expect("nested stage checkout");
    guard
        .disarm(&mut stage, &operation)
        .expect("nested stage reservation settles");

    drop((operation, directory, admitted, authority, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn unix_observed_publication_runs_real_parent_barriers() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
    let source_path = temporary.path().join("observed-stage.bin");
    let destination_path = temporary
        .path()
        .join("destination")
        .join("observed-publication.bin");
    std::fs::write(&source_path, b"observed payload").expect("observed stage");
    let staged_file = File::open(&source_path).expect("retained observed stage");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let destination = root
        .open_directory(&LeafName::new("destination").expect("destination leaf"))
        .expect("destination capability");
    let attempt_id = 1;
    let (sealed_size, sealed_stamp) =
        platform::file_receipt_fields(&staged_file).expect("observed stage revision");
    let mut attempt = platform::prepare_publication(
        attempt_id,
        &staged_file,
        sealed_size,
        sealed_stamp,
        root.inner.handle(),
        OsStr::new("observed-stage.bin"),
        destination.inner.handle(),
        OsStr::new("observed-publication.bin"),
    )
    .expect("prepare observed publication");
    std::fs::rename(&source_path, &destination_path).expect("externally observed publication");
    platform::settle_publication(
        &mut attempt,
        attempt_id,
        &staged_file,
        root.inner.handle(),
        OsStr::new("observed-stage.bin"),
        destination.inner.handle(),
        OsStr::new("observed-publication.bin"),
    )
    .expect("settle observed publication");
    assert!(!attempt.is_attempted());

    drop((staged_file, root, destination));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn unix_publication_destination_barrier_failure_is_permanently_poisoned() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let source_path = temporary.path().join("stage.bin");
    std::fs::write(&source_path, b"destination barrier payload").expect("stage payload");
    let staged_file = File::open(&source_path).expect("retained stage");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let attempt_id = 1;
    let (sealed_size, sealed_stamp) =
        platform::file_receipt_fields(&staged_file).expect("stage revision");
    let mut receipt = platform::prepare_publication(
        attempt_id,
        &staged_file,
        sealed_size,
        sealed_stamp,
        root.inner.handle(),
        OsStr::new("stage.bin"),
        root.inner.handle(),
        OsStr::new("published.bin"),
    )
    .expect("prepare publication");
    platform::rename_no_replace(
        &mut receipt,
        attempt_id,
        root.inner.handle(),
        OsStr::new("stage.bin"),
        &staged_file,
        root.inner.handle(),
        OsStr::new("published.bin"),
    )
    .expect("apply publication");

    {
        let _fault =
            platform::install_publication_directory_sync_test_outcomes([Err(io::ErrorKind::Other)]);
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                root.inner.handle(),
                OsStr::new("stage.bin"),
                root.inner.handle(),
                OsStr::new("published.bin"),
            )
            .is_err()
        );
    }
    {
        let _would_succeed = platform::install_publication_directory_sync_test_outcomes([Ok(())]);
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                root.inner.handle(),
                OsStr::new("stage.bin"),
                root.inner.handle(),
                OsStr::new("published.bin"),
            )
            .is_err(),
            "a consumed destination barrier failure must never be retried into durability",
        );
    }

    drop((staged_file, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn unix_publication_source_barrier_failure_is_permanently_poisoned() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
    let source_path = temporary.path().join("stage.bin");
    std::fs::write(&source_path, b"source barrier payload").expect("stage payload");
    let staged_file = File::open(&source_path).expect("retained stage");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let destination = root
        .open_directory(&LeafName::new("destination").expect("destination leaf"))
        .expect("destination capability");
    let attempt_id = 1;
    let (sealed_size, sealed_stamp) =
        platform::file_receipt_fields(&staged_file).expect("stage revision");
    let mut receipt = platform::prepare_publication(
        attempt_id,
        &staged_file,
        sealed_size,
        sealed_stamp,
        root.inner.handle(),
        OsStr::new("stage.bin"),
        destination.inner.handle(),
        OsStr::new("published.bin"),
    )
    .expect("prepare publication");
    platform::rename_no_replace(
        &mut receipt,
        attempt_id,
        root.inner.handle(),
        OsStr::new("stage.bin"),
        &staged_file,
        destination.inner.handle(),
        OsStr::new("published.bin"),
    )
    .expect("apply publication");

    {
        let _fault = platform::install_publication_directory_sync_test_outcomes([
            Ok(()),
            Err(io::ErrorKind::Other),
        ]);
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                root.inner.handle(),
                OsStr::new("stage.bin"),
                destination.inner.handle(),
                OsStr::new("published.bin"),
            )
            .is_err()
        );
    }
    {
        let _would_succeed =
            platform::install_publication_directory_sync_test_outcomes([Ok(()), Ok(())]);
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                root.inner.handle(),
                OsStr::new("stage.bin"),
                destination.inner.handle(),
                OsStr::new("published.bin"),
            )
            .is_err(),
            "a consumed source barrier failure must never be retried into durability",
        );
    }

    drop((staged_file, destination, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn publication_receipt_rejects_reuse_for_another_mutation() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let source_path = temporary.path().join("stage.bin");
    let other_path = temporary.path().join("other-stage.bin");
    std::fs::write(&source_path, b"payload").expect("stage payload");
    std::fs::write(&other_path, b"other payload").expect("other payload");
    std::fs::create_dir(temporary.path().join("other")).expect("other parent");
    let staged_file = File::open(&source_path).expect("retained stage");
    let other_file = File::open(&other_path).expect("other retained file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let other_parent = root
        .open_directory(&LeafName::new("other").expect("other parent leaf"))
        .expect("other parent capability");
    let attempt_id = 1;
    let (sealed_size, sealed_stamp) =
        platform::file_receipt_fields(&staged_file).expect("stage revision");
    let mut attempt = platform::prepare_publication(
        attempt_id,
        &staged_file,
        sealed_size,
        sealed_stamp,
        root.inner.handle(),
        OsStr::new("stage.bin"),
        root.inner.handle(),
        OsStr::new("published.bin"),
    )
    .expect("prepare reported publication");
    platform::rename_no_replace(
        &mut attempt,
        attempt_id,
        root.inner.handle(),
        OsStr::new("stage.bin"),
        &staged_file,
        root.inner.handle(),
        OsStr::new("published.bin"),
    )
    .expect("reported publication");
    let mut receipt = attempt;
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id,
            &other_file,
            root.inner.handle(),
            OsStr::new("stage.bin"),
            root.inner.handle(),
            OsStr::new("published.bin"),
        )
        .is_err()
    );
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id + 1,
            &staged_file,
            root.inner.handle(),
            OsStr::new("stage.bin"),
            root.inner.handle(),
            OsStr::new("published.bin"),
        )
        .is_err()
    );
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id,
            &staged_file,
            other_parent.inner.handle(),
            OsStr::new("stage.bin"),
            root.inner.handle(),
            OsStr::new("published.bin"),
        )
        .is_err()
    );
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id,
            &staged_file,
            root.inner.handle(),
            OsStr::new("stage.bin"),
            other_parent.inner.handle(),
            OsStr::new("published.bin"),
        )
        .is_err()
    );
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id,
            &staged_file,
            root.inner.handle(),
            OsStr::new("other-stage.bin"),
            root.inner.handle(),
            OsStr::new("published.bin"),
        )
        .is_err()
    );
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id,
            &staged_file,
            root.inner.handle(),
            OsStr::new("stage.bin"),
            root.inner.handle(),
            OsStr::new("other.bin"),
        )
        .is_err()
    );
    assert!(
        platform::settle_publication(
            &mut receipt,
            attempt_id,
            &staged_file,
            root.inner.handle(),
            OsStr::new("stage.bin"),
            root.inner.handle(),
            OsStr::new("published.bin"),
        )
        .is_ok()
    );

    drop((staged_file, other_file, other_parent, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn windows_publication_requires_reported_write_through_receipt() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let staged = test_sealed_stage(&root, b"unreported payload");
    let attempt_id = staged
        .token
        .allocate_publication_attempt()
        .expect("publication attempt identity");
    let mut attempt = platform::prepare_publication(
        attempt_id,
        &staged.file.handle,
        staged.revision.size,
        staged.revision.stamp,
        root.inner.handle(),
        staged.file.name.as_os_str(),
        root.inner.handle(),
        OsStr::new("unreported.bin"),
    )
    .expect("prepare unreported publication");

    assert!(
        platform::settle_publication(
            &mut attempt,
            attempt_id,
            &staged.file.handle,
            root.inner.handle(),
            staged.file.name.as_os_str(),
            root.inner.handle(),
            OsStr::new("unreported.bin"),
        )
        .is_err()
    );
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));

    let staged = test_sealed_stage(&root, b"reported payload");
    let published = require_test_promotion(staged.promote_no_replace(
        &root,
        &root,
        &LeafName::new("reported.bin").expect("reported leaf"),
    ));

    drop((published, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn windows_committed_publication_receipt_rejects_cross_mutation() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("other-parent")).expect("other parent");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let other_parent = root
        .open_directory(&LeafName::new("other-parent").expect("other parent leaf"))
        .expect("other parent capability");
    let staged = test_sealed_stage(&root, b"committed payload");
    let source_name = staged.file.name.clone();
    let destination_name = LeafName::new("committed.bin").expect("destination leaf");
    let attempt_id = staged
        .token
        .allocate_publication_attempt()
        .expect("publication attempt identity");
    let restore_attempt_id = staged
        .token
        .allocate_publication_attempt()
        .expect("restore attempt identity");
    let mut attempt = platform::prepare_publication(
        attempt_id,
        &staged.file.handle,
        staged.revision.size,
        staged.revision.stamp,
        root.inner.handle(),
        source_name.as_os_str(),
        root.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("prepare publication");
    staged
        .token
        .prepare_promotion(
            &root,
            &destination_name,
            attempt_id,
            attempt.clone(),
            None,
            None,
        )
        .expect("track publication attempt");
    platform::rename_no_replace(
        &mut attempt,
        attempt_id,
        root.inner.handle(),
        source_name.as_os_str(),
        &staged.file.handle,
        root.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("report publication");
    staged.token.record_publication(attempt_id, attempt.clone());
    let mut committed = attempt;
    staged
        .token
        .validate_publication_attempt(attempt_id, &committed)
        .expect("validate live publication attempt");
    platform::settle_publication(
        &mut committed,
        attempt_id,
        &staged.file.handle,
        root.inner.handle(),
        source_name.as_os_str(),
        root.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("commit publication");
    staged
        .token
        .record_publication(attempt_id, committed.clone());
    platform::settle_publication(
        &mut committed,
        attempt_id,
        &staged.file.handle,
        root.inner.handle(),
        source_name.as_os_str(),
        root.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("revalidate committed publication");

    let other = test_sealed_stage(&root, b"other payload");
    let wrong_destination = LeafName::new("other-destination.bin").expect("wrong destination");
    let cases = [
        (
            "attempt",
            attempt_id + 1,
            &staged.file.handle,
            root.inner.handle(),
            source_name.as_os_str(),
            root.inner.handle(),
            destination_name.as_os_str(),
        ),
        (
            "staged file",
            attempt_id,
            &other.file.handle,
            root.inner.handle(),
            source_name.as_os_str(),
            root.inner.handle(),
            destination_name.as_os_str(),
        ),
        (
            "source parent",
            attempt_id,
            &staged.file.handle,
            other_parent.inner.handle(),
            source_name.as_os_str(),
            root.inner.handle(),
            destination_name.as_os_str(),
        ),
        (
            "destination parent",
            attempt_id,
            &staged.file.handle,
            root.inner.handle(),
            source_name.as_os_str(),
            other_parent.inner.handle(),
            destination_name.as_os_str(),
        ),
        (
            "source leaf",
            attempt_id,
            &staged.file.handle,
            root.inner.handle(),
            other.file.name.as_os_str(),
            root.inner.handle(),
            destination_name.as_os_str(),
        ),
        (
            "destination leaf",
            attempt_id,
            &staged.file.handle,
            root.inner.handle(),
            source_name.as_os_str(),
            root.inner.handle(),
            wrong_destination.as_os_str(),
        ),
    ];
    for (
        label,
        candidate_attempt,
        candidate_file,
        candidate_source_parent,
        candidate_source_name,
        candidate_destination_parent,
        candidate_destination_name,
    ) in cases
    {
        assert!(
            platform::settle_publication(
                &mut committed,
                candidate_attempt,
                candidate_file,
                candidate_source_parent,
                candidate_source_name,
                candidate_destination_parent,
                candidate_destination_name,
            )
            .is_err(),
            "committed receipt accepted a different {label}",
        );
    }

    assert!(matches!(other.discard(), StageDiscardOutcome::Discarded));
    let (restore_size, restore_stamp) =
        platform::file_receipt_fields(&staged.file.handle).expect("restore revision");
    let mut restore = platform::prepare_publication(
        restore_attempt_id,
        &staged.file.handle,
        restore_size,
        restore_stamp,
        root.inner.handle(),
        destination_name.as_os_str(),
        root.inner.handle(),
        source_name.as_os_str(),
    )
    .expect("prepare stage-binding restoration");
    platform::rename_no_replace(
        &mut restore,
        restore_attempt_id,
        root.inner.handle(),
        destination_name.as_os_str(),
        &staged.file.handle,
        root.inner.handle(),
        source_name.as_os_str(),
    )
    .expect("restore stage binding through retained handle");
    staged
        .token
        .update(StageRegistryPhase::Sealed)
        .expect("release publication tracking after restoration");
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    drop((other_parent, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn admitted_absolute_directories_retain_distinct_physical_bindings() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    let unrelated = temporary.path().join("unrelated");
    for path in [&app_root, &library, &unrelated] {
        std::fs::create_dir(path).expect("create directory");
    }
    let session = acquire_test_root(&app_root);
    {
        let admitted = session
            .admit_absolute_directory_authority(&library)
            .expect("admit library");
        let unrelated = session
            .admit_absolute_directory_authority(&unrelated)
            .expect("admit unrelated directory");
        assert_ne!(
            admitted.filesystem_identity().expect("library identity"),
            unrelated.filesystem_identity().expect("unrelated identity")
        );
        admitted
            .revalidate()
            .expect("original binding remains valid");
    }
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn admitted_absolute_directory_accepts_the_filesystem_root() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    std::fs::create_dir(&app_root).expect("create app root");
    let session = acquire_test_root(&app_root);

    let root = session
        .admit_absolute_directory(Path::new("/"))
        .expect("admit filesystem root");
    match session.admit_absolute_directory_authority_outside_root(Path::new("/")) {
        AbsoluteDirectoryOutsideRootAdmission::Admitted(admitted) => drop(admitted),
        AbsoluteDirectoryOutsideRootAdmission::InsideRoot => {
            panic!("filesystem root is not inside the nested app root")
        }
        AbsoluteDirectoryOutsideRootAdmission::Unavailable(error) => {
            panic!("filesystem root authority unavailable: {error}")
        }
    }

    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn admitted_absolute_directory_accepts_the_volume_root() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    std::fs::create_dir(&app_root).expect("create app root");
    let volume_root = temporary
        .path()
        .ancestors()
        .last()
        .expect("temporary path volume root");
    assert!(volume_root.is_absolute());
    let session = acquire_test_root(&app_root);

    let root = session
        .admit_absolute_directory(volume_root)
        .expect("admit volume root");
    match session.admit_absolute_directory_authority_outside_root(volume_root) {
        AbsoluteDirectoryOutsideRootAdmission::Admitted(admitted) => drop(admitted),
        AbsoluteDirectoryOutsideRootAdmission::InsideRoot => {
            panic!("volume root is not inside the nested app root")
        }
        AbsoluteDirectoryOutsideRootAdmission::Unavailable(error) => {
            panic!("volume root authority unavailable: {error}")
        }
    }

    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn admitted_absolute_directory_rejects_a_case_alias_leaf() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    let alias = temporary.path().join("Library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let session = acquire_test_root(&app_root);

    session
        .admit_absolute_directory_authority(&alias)
        .expect_err("case alias must not satisfy exact absolute admission");

    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn admitted_absolute_directory_refreshes_exact_name_after_parent_change() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    let alias = temporary.path().join("Library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let session = acquire_test_root(&app_root);
    let admitted = session
        .admit_absolute_directory_authority(&library)
        .expect("admit library");

    std::fs::create_dir(temporary.path().join("sibling")).expect("create sibling");
    admitted
        .revalidate()
        .expect("refresh exact proof after sibling change");
    std::fs::rename(&library, &alias).expect("rename admitted leaf");
    admitted
        .revalidate()
        .expect_err("case alias must invalidate the refreshed exact proof");
    assert!(admitted.filesystem_identity().is_err());

    drop(admitted);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn admitted_absolute_directory_rejects_replaced_leaf_binding() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    let displaced = temporary.path().join("displaced-library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let session = acquire_test_root(&app_root);
    {
        let admitted = session
            .admit_absolute_directory_authority(&library)
            .expect("admit library");
        std::fs::rename(&library, &displaced).expect("displace admitted library");
        std::fs::create_dir(&library).expect("create replacement library");
        assert!(admitted.revalidate().is_err());
        assert!(admitted.filesystem_identity().is_err());
    }
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn admitted_identity_refuses_displaced_ancestry_and_root_authority() {
    for binding in ["ancestor", "root", "lease"] {
        let temporary = crate::test_tempdir().unwrap();
        let app = temporary.path().join("app");
        let parent = temporary.path().join("external");
        let library = parent.join("library");
        std::fs::create_dir(&app).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(library.join("payload"), b"original").unwrap();
        let session = acquire_test_root(&app);
        let admitted = session
            .admit_absolute_directory_authority(&library)
            .unwrap();
        let expected = admitted.filesystem_identity().unwrap();
        let target = match binding {
            "ancestor" => parent.clone(),
            "root" => app.clone(),
            "lease" => app.join(ROOT_LEASE_NAME),
            _ => unreachable!(),
        };
        let displaced = temporary.path().join("displaced");
        std::fs::rename(&target, &displaced).unwrap();
        if binding == "lease" {
            std::fs::write(&target, b"replacement").unwrap();
        } else {
            std::fs::create_dir(&target).unwrap();
        }
        let result = admitted.filesystem_identity();
        if binding == "lease" {
            assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
            std::fs::remove_file(&target).unwrap();
        } else {
            std::fs::remove_dir(&target).unwrap();
        }
        std::fs::rename(&displaced, &target).unwrap();
        assert_eq!(std::fs::read(library.join("payload")).unwrap(), b"original");
        let restored = admitted.filesystem_identity();
        drop(admitted);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        assert!(result.is_err(), "accepted displaced {binding}");
        assert_eq!(restored.unwrap(), expected);
    }
}

#[test]
fn admitted_identity_refuses_revoked_authority() {
    let temporary = crate::test_tempdir().unwrap();
    let session = acquire_test_root(temporary.path());
    let admitted = session
        .admit_absolute_directory_authority(temporary.path())
        .unwrap();
    admitted.filesystem_identity().unwrap();
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(admitted.filesystem_identity().is_err());
}

#[cfg(unix)]
#[test]
fn admitted_root_acquisition_never_creates_or_leases_a_path_replacement() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    let displaced = temporary.path().join("displaced-library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let session = acquire_test_root(&app_root);
    {
        let admitted = session
            .admit_absolute_directory_authority(&library)
            .expect("admit library");
        std::fs::rename(&library, &displaced).expect("displace admitted library");

        assert!(admitted.acquire_root_session().is_err());
        assert!(!library.exists(), "acquisition recreated a missing path");

        std::fs::create_dir(&library).expect("create replacement library");
        assert!(admitted.acquire_root_session().is_err());
        assert!(
            !library.join(ROOT_LEASE_NAME).exists(),
            "acquisition touched the replacement directory"
        );
    }
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn admitted_root_session_clones_its_detached_root_capability() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app_root = temporary.path().join("app");
    let library = temporary.path().join("library");
    std::fs::create_dir(&app_root).expect("create app root");
    std::fs::create_dir(&library).expect("create library root");
    let app_session = acquire_test_root(&app_root);
    let admitted = app_session
        .admit_absolute_directory_authority(&library)
        .expect("admit library");
    let expected = admitted.filesystem_identity().expect("admitted identity");
    let admitted_session = match admitted
        .acquire_root_session()
        .expect("start admitted root acquisition")
    {
        RootSessionAcquireOutcome::Acquired(session) => session,
        RootSessionAcquireOutcome::NoEffect(error) => {
            panic!("admitted root acquisition failed: {error}")
        }
        RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            let error = obligation.error().to_string();
            assert!(
                obligation.cleanup().is_ok(),
                "admitted root acquisition cleanup remained unsettled"
            );
            panic!("admitted root acquisition was unverified: {error}");
        }
    };
    admitted_session
        .validate_retained_authority()
        .expect("retained admitted root authority");
    assert_eq!(
        admitted_session
            .root()
            .expect("clone detached root")
            .identity()
            .expect("detached root identity")
            .filesystem_identity(),
        expected
    );
    drop(admitted_session);
    assert!(matches!(
        acquire_test_root(&library).revoke(),
        RootRevokeOutcome::Revoked
    ));
    assert!(matches!(app_session.revoke(), RootRevokeOutcome::Revoked));
}

fn park_preservation_test_file(root: &Directory) -> ParkedFile {
    park_test_file(root, "record.bin", "record.preserved", b"payload")
}

fn park_test_file(root: &Directory, name: &str, park_name: &str, payload: &[u8]) -> ParkedFile {
    let file = root
        .open_file(&LeafName::new(name).expect("record leaf"))
        .expect("record capability");
    let revision = file.revision().expect("record revision");
    let digest: [u8; 32] = Sha256::digest(payload).into();
    let request = file.park_request(ExpectedFileContent::new(revision, digest));
    match root.park_file_as(request, LeafName::new(park_name).expect("preserved leaf")) {
        FileParkOutcome::Parked(parked) => parked,
        FileParkOutcome::NoEffect { error, .. } => {
            panic!("preservation park had no effect: {error}")
        }
        FileParkOutcome::Preserved { error, .. } => {
            panic!("preservation park retained changed content: {error}")
        }
        FileParkOutcome::AppliedUnverified(obligation) => {
            panic!("preservation park was not verified: {}", obligation.error())
        }
    }
}

fn restored_file_park_obligation(
    root: &Directory,
    name: &str,
    park_name: &str,
    payload: &[u8],
) -> FileParkObligation {
    let name = LeafName::new(name).expect("record leaf");
    let park_name = LeafName::new(park_name).expect("park leaf");
    let file = root.open_file(&name).expect("record capability");
    let revision = file.revision().expect("record revision");
    let digest: [u8; 32] = Sha256::digest(payload).into();
    let request = file.park_request(ExpectedFileContent::new(revision, digest));
    let authority = root.authority().expect("root authority");
    let operation = authority.enter().expect("park operation");
    let cleanup =
        platform::open_parked_file(root.inner.handle(), name.as_os_str(), request.file.identity)
            .expect("retained cleanup handle");
    let token = authority
        .reserve_file_park(&operation, &request, park_name.clone(), cleanup)
        .expect("park reservation");
    let mut guard = authority
        .take_file_park(&operation, &token)
        .expect("park record");
    match platform::park_file_no_replace(
        root.inner.handle(),
        name.as_os_str(),
        &request.file.handle,
        request.file.identity,
        park_name.as_os_str(),
        &guard.record().cleanup,
    ) {
        Ok(()) => {}
        Err(platform::ParkFileError::NoEffect(error)) => {
            panic!("test file park had no effect: {error}")
        }
        Err(platform::ParkFileError::AppliedUnverified(error)) => {
            panic!("test file park was unverified: {error}")
        }
    }
    {
        let record = guard.record_mut();
        platform::restore_parked_file(
            record.parent.inner.handle(),
            record.name.as_os_str(),
            &mut record.cleanup,
            record.identity,
            record.original_name.as_os_str(),
        )
        .expect("restore parked file");
    }
    drop(guard);
    drop(operation);
    FileParkObligation {
        error: io::Error::other("test restored park requires settlement"),
        request: Some(request),
        token,
        park_name,
        phase: FileParkPhase::RestoringRejectedReceipt,
        digest_verified: false,
        restored_proof_pause: None,
    }
}

#[test]
fn restored_park_refreshes_its_request_revision() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("current.bin"), b"current payload")
        .expect("current payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let obligation =
        restored_file_park_obligation(&root, "current.bin", "current.parked", b"current payload");
    let request = match obligation.reconcile() {
        FileParkResolution::NoEffect(request) => request,
        FileParkResolution::Parked(_) => panic!("restored file remained parked"),
        FileParkResolution::Preserved { error, .. } => {
            panic!("unchanged restored file was only preserved: {error}")
        }
        FileParkResolution::Indeterminate(obligation) => {
            panic!(
                "restored file remained indeterminate: {}",
                obligation.error()
            )
        }
    };
    let (file, revision, _) = request.into_parts();
    file.validate_revision(&revision)
        .expect("restored request carries the refreshed revision");
    drop((file, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn restored_park_preserves_stable_digest_drift_without_delete_authority() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("changed.bin"), b"original payload")
        .expect("original payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let obligation =
        restored_file_park_obligation(&root, "changed.bin", "changed.parked", b"original payload");
    std::fs::write(temporary.path().join("changed.bin"), b"modified payload")
        .expect("modified payload");
    let file = match obligation.reconcile() {
        FileParkResolution::Preserved { file, .. } => file,
        FileParkResolution::NoEffect(_) => {
            panic!("changed restored file returned deletion authority")
        }
        FileParkResolution::Parked(_) => panic!("changed restored file remained parked"),
        FileParkResolution::Indeterminate(obligation) => {
            panic!(
                "stable changed file remained indeterminate: {}",
                obligation.error()
            )
        }
    };
    assert_eq!(
        std::fs::read(temporary.path().join("changed.bin")).expect("preserved payload"),
        b"modified payload",
    );
    drop(file);
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn restored_park_retains_racing_content_until_a_stable_preservation_proof() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("racing.bin"), b"original payload")
        .expect("original payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let mut obligation =
        restored_file_park_obligation(&root, "racing.bin", "racing.parked", b"original payload");
    let hashed = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    obligation.restored_proof_pause = Some(RestoredFileProofPause {
        hashed: Arc::clone(&hashed),
        resume: Arc::clone(&resume),
    });
    let settlement = std::thread::spawn(move || obligation.reconcile());
    hashed.wait();
    std::fs::write(temporary.path().join("racing.bin"), b"modified payload")
        .expect("racing payload");
    resume.wait();
    let obligation = match settlement.join().expect("settlement thread") {
        FileParkResolution::Indeterminate(obligation) => obligation,
        FileParkResolution::NoEffect(_) => {
            panic!("racing restored file returned deletion authority")
        }
        FileParkResolution::Preserved { .. } => {
            panic!("racing restored file was classified from unequal receipts")
        }
        FileParkResolution::Parked(_) => panic!("racing restored file remained parked"),
    };
    let file = match obligation.reconcile() {
        FileParkResolution::Preserved { file, .. } => file,
        FileParkResolution::NoEffect(_) => {
            panic!("changed restored file returned deletion authority")
        }
        FileParkResolution::Indeterminate(obligation) => {
            panic!(
                "stable retry remained indeterminate: {}",
                obligation.error()
            )
        }
        FileParkResolution::Parked(_) => panic!("stable retry remained parked"),
    };
    drop(file);
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn restored_park_retains_share_locked_content_until_settlement() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("locked.bin"), b"original payload")
        .expect("original payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let obligation =
        restored_file_park_obligation(&root, "locked.bin", "locked.parked", b"original payload");

    std::fs::write(temporary.path().join("locked.bin"), b"modified payload")
        .expect_err("retained Windows cleanup authority must refuse a conflicting writer");
    match obligation.reconcile() {
        FileParkResolution::NoEffect(request) => drop(request),
        FileParkResolution::Preserved { error, .. } => {
            panic!("share-locked content unexpectedly drifted: {error}")
        }
        FileParkResolution::Indeterminate(obligation) => {
            panic!(
                "share-locked restored file remained indeterminate: {}",
                obligation.error()
            )
        }
        FileParkResolution::Parked(_) => panic!("restored file remained parked"),
    }
    assert_eq!(
        std::fs::read(temporary.path().join("locked.bin")).expect("share-locked payload"),
        b"original payload",
    );
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

fn park_test_directory(root: &Directory, name: &str, park_name: &str) -> ParkedDirectory {
    let directory = root
        .open_directory(&LeafName::new(name).expect("directory leaf"))
        .expect("directory capability");
    match directory.park_as(LeafName::new(park_name).expect("park leaf")) {
        DirectoryParkOutcome::Applied(parked) => parked,
        DirectoryParkOutcome::NoEffect { error, .. } => {
            panic!("directory park had no effect: {error}")
        }
        DirectoryParkOutcome::AppliedUnverified(obligation) => {
            panic!("directory park was not verified: {}", obligation.error())
        }
    }
}

fn create_test_directories(root: &Path, relative: &[&str]) {
    for path in relative {
        std::fs::create_dir(root.join(path)).expect("test directory");
    }
}

fn test_sealed_stage(root: &Directory, bytes: &[u8]) -> SealedStagedFile {
    let mut staged = match root.create_stage() {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => panic!("stage creation failed: {error}"),
        FileCreateOutcome::AppliedUnverified(obligation) => {
            panic!("stage creation was not verified: {}", obligation.error())
        }
    };
    staged.write_all(bytes).expect("stage bytes");
    staged.seal().expect("sealed stage")
}

fn test_recoverable_stage(
    root: &Directory,
    destination: &LeafName,
    bytes: &[u8],
) -> (SealedStagedFile, RecoveryRegistration) {
    let mut staged = match root.create_recoverable_stage(destination) {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => {
            panic!("recovery stage creation failed: {error}")
        }
        FileCreateOutcome::AppliedUnverified(obligation) => {
            panic!(
                "recovery stage creation was not verified: {}",
                obligation.error()
            )
        }
    };
    staged.write_all(bytes).expect("recovery stage bytes");
    let sealed = staged.seal().expect("sealed recovery stage");
    let registration = root
        .authority()
        .expect("recovery authority")
        .operations
        .lock()
        .expect("recovery state")
        .stages
        .get(&sealed.token.id)
        .expect("registered recovery stage")
        .recovery
        .expect("recovery registration");
    (sealed, registration)
}

fn persist_test_recovery_fixture(
    session: &RootSession,
    root_path: &Path,
    operation_id: [u8; 16],
    destination: &str,
    phase: RecoveryPhase,
    payload: &[u8],
    create_stage: bool,
) -> (RecoveryRegistration, RecoveryName) {
    persist_test_recovery_fixture_in(
        session,
        root_path,
        operation_id,
        Vec::new(),
        destination,
        phase,
        payload,
        create_stage,
    )
}

fn persist_test_recovery_fixture_in(
    session: &RootSession,
    root_path: &Path,
    operation_id: [u8; 16],
    destination_parent: Vec<RecoveryName>,
    destination: &str,
    phase: RecoveryPhase,
    payload: &[u8],
    create_stage: bool,
) -> (RecoveryRegistration, RecoveryName) {
    let stage = recovery_stage_leaf(operation_id);
    let stage_parent = destination_parent
        .iter()
        .fold(root_path.to_path_buf(), |path, component| {
            path.join(component.as_str())
        });
    let mut record = RecoveryRecord {
        operation_id,
        phase: RecoveryPhase::StagePrepared,
        destination_parent,
        destination_leaf: RecoveryName::new_exact(destination).expect("recovery destination"),
        old: None,
        new: None,
    };
    let mut journal =
        RecoveryJournal::load(&session.authority.lease).expect("load recovery fixture journal");
    let registration = journal.reserve(&record).expect("reserve recovery fixture");
    journal
        .create_reserved(&session.authority.lease, registration, record.clone())
        .expect("persist prepared recovery fixture");
    if create_stage {
        let path = stage_parent.join(stage.as_str());
        std::fs::write(&path, payload).expect("write recovery fixture stage");
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open recovery fixture stage")
            .sync_all()
            .expect("sync recovery fixture stage");
    }
    if phase != RecoveryPhase::StagePrepared {
        record.phase = RecoveryPhase::StageSealed;
        record.new = Some(recovery::RecoveryFileProof {
            size: payload.len() as u64,
            sha256: Sha256::digest(payload).into(),
        });
        journal
            .advance(&session.authority.lease, registration, record.clone())
            .expect("persist sealed recovery fixture");
    }
    if phase != record.phase {
        if phase == RecoveryPhase::RemoveCommitted {
            record.phase = RecoveryPhase::PublishPrepared;
            journal
                .advance(&session.authority.lease, registration, record.clone())
                .expect("persist prepared publication fixture");
        }
        record.phase = phase;
        journal
            .advance(&session.authority.lease, registration, record)
            .expect("persist final recovery fixture phase");
    }
    (registration, stage)
}

fn persist_test_replacement_fixture(
    session: &RootSession,
    root_path: &Path,
    operation_id: [u8; 16],
    destination: &str,
    phase: RecoveryPhase,
    old_payload: &[u8],
    new_payload: &[u8],
    carriers: [Option<&[u8]>; 3],
) -> (RecoveryRegistration, RecoveryName, RecoveryName) {
    let stage = recovery_stage_leaf(operation_id);
    let park = recovery_park_leaf(operation_id);
    let old = recovery::RecoveryFileProof {
        size: old_payload.len() as u64,
        sha256: Sha256::digest(old_payload).into(),
    };
    let new = recovery::RecoveryFileProof {
        size: new_payload.len() as u64,
        sha256: Sha256::digest(new_payload).into(),
    };
    let mut record = RecoveryRecord {
        operation_id,
        phase: RecoveryPhase::StagePrepared,
        destination_parent: Vec::new(),
        destination_leaf: RecoveryName::new_exact(destination).expect("recovery destination"),
        old: Some(old),
        new: None,
    };
    let mut journal =
        RecoveryJournal::load(&session.authority.lease).expect("load replacement journal");
    let registration = journal
        .reserve(&record)
        .expect("reserve replacement fixture");
    journal
        .create_reserved(&session.authority.lease, registration, record.clone())
        .expect("persist prepared replacement fixture");
    let phases: &[RecoveryPhase] = match phase {
        RecoveryPhase::StagePrepared => &[],
        RecoveryPhase::StageSealed => &[RecoveryPhase::StageSealed],
        RecoveryPhase::ReplacePrepared => {
            &[RecoveryPhase::StageSealed, RecoveryPhase::ReplacePrepared]
        }
        RecoveryPhase::PublishPrepared => &[
            RecoveryPhase::StageSealed,
            RecoveryPhase::ReplacePrepared,
            RecoveryPhase::PublishPrepared,
        ],
        RecoveryPhase::RemovePrepared => {
            &[RecoveryPhase::StageSealed, RecoveryPhase::RemovePrepared]
        }
        RecoveryPhase::RemoveCommitted => &[
            RecoveryPhase::StageSealed,
            RecoveryPhase::ReplacePrepared,
            RecoveryPhase::PublishPrepared,
            RecoveryPhase::RemoveCommitted,
        ],
    };
    for &phase in phases {
        record.phase = phase;
        if phase == RecoveryPhase::StageSealed {
            record.new = Some(new);
        }
        journal
            .advance(&session.authority.lease, registration, record.clone())
            .expect("persist replacement fixture phase");
    }
    for (name, payload) in [
        (stage.as_str(), carriers[0]),
        (destination, carriers[1]),
        (park.as_str(), carriers[2]),
    ] {
        let Some(payload) = payload else { continue };
        let path = root_path.join(name);
        std::fs::write(&path, payload).expect("write replacement fixture carrier");
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open replacement fixture carrier")
            .sync_all()
            .expect("sync replacement fixture carrier");
    }
    (registration, stage, park)
}

fn test_file_identity(path: &Path) -> platform::Identity {
    let file = File::open(path).expect("open test identity carrier");
    platform::file_identity(&file).expect("test carrier identity")
}

#[cfg(unix)]
fn assert_test_recovery_terminal(
    session: &RootSession,
    registration: RecoveryRegistration,
    _windows_phase: RecoveryPhase,
    _windows_files: &[platform::Identity],
) {
    let state = session.authority.operations.lock().expect("recovery state");
    assert!(state.recovery.record(registration).is_none());
    assert!(
        state
            .recovery_orphans
            .iter()
            .all(|orphan| orphan.registration != registration)
    );
}

#[cfg(windows)]
fn assert_test_recovery_terminal(
    session: &RootSession,
    registration: RecoveryRegistration,
    windows_phase: RecoveryPhase,
    windows_files: &[platform::Identity],
) {
    let state = session.authority.operations.lock().expect("recovery state");
    assert_eq!(
        state
            .recovery
            .record(registration)
            .expect("retained Windows recovery record")
            .phase,
        windows_phase
    );
    assert!(
        state
            .recovery_orphans
            .iter()
            .find(|orphan| orphan.registration == registration)
            .expect("retained Windows recovery orphan")
            .files
            .as_slice()
            == windows_files
    );
}

fn test_publication_attempt(
    staged: &SealedStagedFile,
    destination: &Directory,
    destination_name: &LeafName,
) -> (u64, platform::PublicationReceipt) {
    let attempt_id = staged
        .token
        .allocate_publication_attempt()
        .expect("publication attempt identity");
    let receipt = platform::prepare_publication(
        attempt_id,
        &staged.file.handle,
        staged.revision.size,
        staged.revision.stamp,
        staged.file.parent.inner.handle(),
        staged.file.name.as_os_str(),
        destination.inner.handle(),
        destination_name.as_os_str(),
    )
    .expect("publication attempt");
    (attempt_id, receipt)
}

fn require_test_promotion(outcome: FilePromotionOutcome) -> FileCapability {
    match outcome {
        FilePromotionOutcome::Applied(file) => file,
        FilePromotionOutcome::NoEffect {
            error,
            item: staged,
        } => {
            assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
            panic!("stage promotion had no effect: {error}");
        }
        FilePromotionOutcome::AppliedUnverified(obligation) => {
            let initial_error = obligation.error().to_string();
            match (*obligation).reconcile() {
                FilePromotionResolution::Applied(file) => file,
                FilePromotionResolution::NoEffect(staged) => {
                    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
                    panic!("stage promotion reconciled to no effect: {initial_error}");
                }
                FilePromotionResolution::Indeterminate(obligation) => {
                    panic!(
                        "stage promotion remained indeterminate after reconciliation: {}; initial error: {initial_error}",
                        obligation.error()
                    )
                }
            }
        }
    }
}

fn require_test_promotion_no_effect(outcome: FilePromotionOutcome) -> SealedStagedFile {
    match outcome {
        FilePromotionOutcome::NoEffect { item: staged, .. } => staged,
        FilePromotionOutcome::Applied(_) => panic!("stage promotion unexpectedly applied"),
        FilePromotionOutcome::AppliedUnverified(obligation) => match (*obligation).reconcile() {
            FilePromotionResolution::NoEffect(staged) => staged,
            FilePromotionResolution::Applied(_) => {
                panic!("stage promotion reconciliation unexpectedly applied")
            }
            FilePromotionResolution::Indeterminate(obligation) => panic!(
                "stage promotion remained indeterminate: {}",
                obligation.error()
            ),
        },
    }
}

fn require_test_stage_discard(outcome: StageDiscardOutcome) {
    match outcome {
        StageDiscardOutcome::Discarded => {}
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            let initial_error = obligation.error().to_string();
            match obligation.reconcile() {
                StageDiscardResolution::Discarded => {}
                StageDiscardResolution::Indeterminate(obligation) => panic!(
                    "stage discard remained indeterminate: {}; initial error: {initial_error}",
                    obligation.error()
                ),
            }
        }
    }
}

fn discard_test_park_registration(mut parked: ParkedFile) {
    let authority = parked.authority().expect("park authority");
    let operation = authority
        .enter_file_park(&parked.token)
        .expect("park cleanup operation");
    let guard = authority
        .take_file_park(&operation, &parked.token)
        .expect("park cleanup guard");
    guard.disarm(&mut parked.token, &operation);
}

fn test_file_move_obligation(
    file: FileCapability,
    destination: Directory,
    destination_name: &str,
    reported_success: bool,
) -> FileMoveObligation {
    let authority = file.parent.authority().expect("move authority");
    let destination_name = LeafName::new(destination_name).expect("destination leaf");
    let token = {
        let operation = authority.enter().expect("move reservation operation");
        MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: file.parent.clone(),
                name: file.name.clone(),
            },
            NamespaceLeaf {
                parent: destination.clone(),
                name: destination_name.clone(),
            },
            None,
            Some(file.identity),
        )
        .expect("move effect reservation")
    };
    FileMoveObligation {
        error: io::Error::other("test move requires settlement"),
        file: Some(file),
        destination,
        destination_name,
        reported_success,
        token,
    }
}

fn test_directory_move_obligation(
    directory: Directory,
    destination: Directory,
    destination_name: &str,
    reported_success: bool,
) -> DirectoryMoveObligation {
    let authority = directory.authority().expect("move authority");
    let binding = directory.inner.parent.as_ref().expect("non-root directory");
    let source_name = LeafName::new(binding.name.clone()).expect("source leaf");
    let destination_name = LeafName::new(destination_name).expect("destination leaf");
    let token = {
        let operation = authority.enter().expect("move reservation operation");
        MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: binding.directory.clone(),
                name: source_name,
            },
            NamespaceLeaf {
                parent: destination.clone(),
                name: destination_name.clone(),
            },
            Some(directory.inner.identity.physical),
            None,
        )
        .expect("move effect reservation")
    };
    DirectoryMoveObligation {
        error: io::Error::other("test directory move requires settlement"),
        directory: Some(directory),
        destination,
        destination_name,
        reported_success,
        token,
    }
}

fn claim_no_effect(receipt: FileMoveReceipt) -> FileCapability {
    match receipt.claim() {
        FileMoveReceiptOutcome::NoEffect(file) => file,
        FileMoveReceiptOutcome::Applied(_) => panic!("test move unexpectedly applied"),
        FileMoveReceiptOutcome::Pending(_) => panic!("test move remained pending"),
    }
}

#[test]
fn effect_owner_rejects_sibling_anchor_and_returns_the_move_carrier() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("first")).expect("first anchor");
    std::fs::create_dir(temporary.path().join("second")).expect("second anchor");
    std::fs::write(temporary.path().join("second/source.bin"), b"source").expect("source file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let first = root
        .open_directory(&LeafName::new("first").expect("first leaf"))
        .expect("first capability");
    let second = root
        .open_directory(&LeafName::new("second").expect("second leaf"))
        .expect("second capability");
    let first_owner = first.create_effect_owner().expect("first owner");
    let second_owner = second.create_effect_owner().expect("second owner");
    let file = second
        .open_file(&LeafName::new("source.bin").expect("source leaf"))
        .expect("source capability");
    let obligation = test_file_move_obligation(file, second.clone(), "target.bin", false);

    let failure = first_owner
        .retain_file_move(obligation)
        .expect_err("sibling owner must reject the move");
    assert_eq!(failure.error().kind(), io::ErrorKind::PermissionDenied);
    let (_, obligation) = failure.into_parts();
    let receipt = second_owner
        .retain_file_move(obligation)
        .expect("correct owner retains returned carrier");
    second_owner.settle().expect("settle returned carrier");
    let file = claim_no_effect(receipt);
    assert_eq!(file.read_bounded(16).expect("source bytes"), b"source");
    assert!(second_owner.require_settled().is_ok());

    drop((file, first_owner, second_owner, first, second, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn dropped_move_receipts_remain_owned_until_explicit_settlement() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    std::fs::write(temporary.path().join("domain/before.bin"), b"before").expect("before file");
    std::fs::write(temporary.path().join("domain/after.bin"), b"after").expect("after file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");

    let before = domain
        .open_file(&LeafName::new("before.bin").expect("before leaf"))
        .expect("before capability");
    let before = owner
        .retain_file_move(test_file_move_obligation(
            before,
            domain.clone(),
            "before-target.bin",
            false,
        ))
        .expect("retain before-terminal receipt");
    drop(before);
    assert!(owner.require_settled().is_err());
    owner.settle().expect("settle abandoned pending receipt");
    assert!(owner.require_settled().is_ok());

    let after = domain
        .open_file(&LeafName::new("after.bin").expect("after leaf"))
        .expect("after capability");
    let after = owner
        .retain_file_move(test_file_move_obligation(
            after,
            domain.clone(),
            "after-target.bin",
            false,
        ))
        .expect("retain after-terminal receipt");
    owner.settle().expect("produce terminal result");
    assert!(owner.require_settled().is_err());
    drop(after);
    assert!(owner.require_settled().is_err());
    owner.settle().expect("dispose abandoned terminal result");
    assert!(owner.require_settled().is_ok());

    drop((owner, domain, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn settlement_extraction_preserves_barriers_capacity_and_receipt_abandonment() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    for index in 0..MAX_EFFECTS_PER_OWNER {
        std::fs::write(
            temporary
                .path()
                .join("domain")
                .join(format!("source-{index}.bin")),
            b"source",
        )
        .expect("unique source file");
    }
    std::fs::write(
        temporary.path().join("domain/overflow-source.bin"),
        b"overflow source",
    )
    .expect("overflow source file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");
    let first_file = domain
        .open_file(&LeafName::new("source-0.bin").expect("source leaf"))
        .expect("source capability");
    let first = owner
        .retain_file_move(test_file_move_obligation(
            first_file,
            domain.clone(),
            "first-target.bin",
            false,
        ))
        .expect("retain first move");
    let extracted = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    *owner
        .state
        .settlement_pause
        .lock()
        .expect("settlement pause") = Some(EffectOwnerSettlementPause {
        extracted: extracted.clone(),
        resume: resume.clone(),
    });
    let settling_owner = owner.clone();
    let settlement = std::thread::spawn(move || settling_owner.settle());
    extracted.wait();

    assert!(owner.has_pending());
    assert_eq!(
        owner
            .require_settled()
            .expect_err("in-flight settlement remains pending")
            .kind(),
        io::ErrorKind::WouldBlock,
    );
    assert_eq!(
        owner
            .settle()
            .expect_err("a second settlement cannot overtake the first")
            .kind(),
        io::ErrorKind::WouldBlock,
    );
    let authority = owner.state.authority.upgrade().expect("owner authority");
    {
        let state = authority.operations.lock().expect("operation state");
        assert!(
            state
                .active_effect_owners
                .get(&owner.state.id)
                .is_some_and(|active| Arc::ptr_eq(active, &owner.state))
        );
    }

    let mut queued = Vec::with_capacity(MAX_EFFECTS_PER_OWNER - 1);
    for index in 1..MAX_EFFECTS_PER_OWNER {
        let file = domain
            .open_file(&LeafName::new(format!("source-{index}.bin")).expect("source leaf"))
            .expect("source capability");
        queued.push(
            owner
                .retain_file_move(test_file_move_obligation(
                    file,
                    domain.clone(),
                    &format!("queued-target-{index}.bin"),
                    false,
                ))
                .expect("in-flight capacity retains only the remaining permits"),
        );
    }
    let overflow_file = domain
        .open_file(&LeafName::new("overflow-source.bin").expect("source leaf"))
        .expect("overflow source capability");
    let failure = owner
        .retain_file_move(test_file_move_obligation(
            overflow_file,
            domain.clone(),
            "overflow-target.bin",
            false,
        ))
        .expect_err("in-flight effects count toward owner capacity");
    assert_eq!(failure.error().kind(), io::ErrorKind::WouldBlock);
    let (_, mut overflow) = failure.into_parts();
    let overflow_file = overflow.file.take().expect("returned overflow file");
    let operation = authority.enter().expect("overflow settlement operation");
    overflow
        .token
        .settle(&operation)
        .expect("settle returned overflow token");
    drop((operation, overflow_file, overflow));

    drop(first);
    for receipt in queued {
        drop(receipt);
    }
    resume.wait();
    settlement
        .join()
        .expect("settlement thread")
        .expect("first settlement");
    assert!(owner.has_pending());
    {
        let records = owner.state.effects.lock().expect("owner records");
        assert!(!records.settling);
        assert_eq!(records.in_flight, 0);
        assert_eq!(records.effects.len(), MAX_EFFECTS_PER_OWNER - 1);
    }
    owner.settle().expect("settle queued abandoned receipts");
    assert!(owner.require_settled().is_ok());

    drop((authority, owner, domain, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn live_pending_or_terminal_move_receipt_blocks_drain_until_claimed() {
    for (case, terminal) in [("pending", false), ("terminal", true)] {
        let temporary = crate::test_tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/source.bin"), b"source").expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let receipt = owner
            .retain_file_move(test_file_move_obligation(
                file,
                domain.clone(),
                "target.bin",
                false,
            ))
            .expect("retain move");
        if terminal {
            owner.settle().expect("produce terminal result");
        }
        drop((owner, domain, root));

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("live {case} receipt did not block revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock, "{case}");
        let file = claim_no_effect(receipt);
        assert_eq!(
            file.read_bounded(16).expect("source bytes"),
            b"source",
            "{case}"
        );
        drop(file);
        assert!(
            matches!(refusal.retry(), RootRevokeOutcome::Revoked),
            "{case}"
        );
    }
}

#[test]
fn dropped_terminal_move_receipt_is_reclaimed_during_drain() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    std::fs::write(temporary.path().join("domain/source.bin"), b"source").expect("source file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");
    let file = domain
        .open_file(&LeafName::new("source.bin").expect("source leaf"))
        .expect("source capability");
    let receipt = owner
        .retain_file_move(test_file_move_obligation(
            file,
            domain.clone(),
            "target.bin",
            false,
        ))
        .expect("retain move");
    owner.settle().expect("produce terminal result");
    drop(receipt);
    assert!(owner.require_settled().is_err());
    drop((owner, domain, root));

    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn live_empty_effect_owner_blocks_terminal_drain_until_dropped() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");
    assert!(owner.require_settled().is_ok());
    drop((domain, root));

    let refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("live empty owner did not block revocation: {outcome:?}"),
    };
    assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
    drop(owner);
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
}

#[test]
fn raw_file_and_directory_parks_refuse_drain_until_settled() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
    std::fs::create_dir(temporary.path().join("folder")).expect("test directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked_file = park_preservation_test_file(&root);
    let parked_directory = park_test_directory(&root, "folder", "folder.parked");
    drop(root);

    let file_refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("raw file park did not refuse revocation: {outcome:?}"),
    };
    assert_eq!(file_refusal.error().kind(), io::ErrorKind::WouldBlock);
    parked_file
        .acknowledge_preserved()
        .expect("settle raw file park");

    let directory_refusal = match file_refusal.retry() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("raw directory park did not refuse revocation: {outcome:?}"),
    };
    assert_eq!(directory_refusal.error().kind(), io::ErrorKind::WouldBlock);
    let directory = match parked_directory.restore() {
        DirectoryRestoreOutcome::Applied(directory) => directory,
        DirectoryRestoreOutcome::NoEffect { error, .. } => {
            panic!("raw directory restore had no effect: {error}")
        }
        DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
            panic!(
                "raw directory restore was not verified: {}",
                obligation.error()
            )
        }
    };
    drop(directory);
    assert!(matches!(
        directory_refusal.retry(),
        RootRevokeOutcome::Revoked
    ));
}

#[test]
fn effect_owner_removes_a_nonempty_parked_directory_tree() {
    let temporary = crate::test_tempdir().expect("temporary root");
    create_test_directories(
        temporary.path(),
        &[
            "domain",
            "domain/victim",
            "domain/victim/nested",
            "domain/victim/nested/deeper",
            "domain/uncertain",
            "domain/uncertain/nested",
        ],
    );
    std::fs::write(
        temporary
            .path()
            .join("domain/victim/nested/deeper/payload.bin"),
        b"payload",
    )
    .expect("nested payload");
    std::fs::write(
        temporary.path().join("domain/uncertain/nested/payload.bin"),
        b"payload",
    )
    .expect("uncertain payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");
    let parked = park_test_directory(&domain, "victim", "victim.deleted");
    let uncertain = park_test_directory(&domain, "uncertain", "uncertain.deleted");

    owner
        .retain_parked_directory_tree_removal(RetainedDirectoryTreeRemoval::new(parked))
        .expect("retain tree removal");
    owner
        .retain_directory_tree_removal(DirectoryTreeRemovalObligation {
            error: io::Error::other("test tree removal requires reconciliation"),
            parked: Some(uncertain),
        })
        .expect("retain indeterminate tree removal");
    owner.settle().expect("settle tree removal");
    owner.require_settled().expect("tree removal settled");
    assert!(!temporary.path().join("domain/victim").exists());
    assert!(!temporary.path().join("domain/victim.deleted").exists());
    assert!(!temporary.path().join("domain/uncertain").exists());
    assert!(!temporary.path().join("domain/uncertain.deleted").exists());

    drop((owner, domain, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn parked_tree_removal_preserves_the_recreated_canonical_binding() {
    let temporary = crate::test_tempdir().expect("temporary root");
    create_test_directories(temporary.path(), &["victim", "victim/nested"]);
    std::fs::write(temporary.path().join("victim/nested/old.bin"), b"old").expect("old payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_directory(&root, "victim", "victim.deleted");
    std::fs::create_dir(temporary.path().join("victim")).expect("replacement canonical root");
    std::fs::write(temporary.path().join("victim/new.bin"), b"new")
        .expect("replacement canonical payload");

    assert!(matches!(
        parked.remove_tree(),
        DirectoryTreeRemovalOutcome::Removed
    ));
    assert_eq!(
        std::fs::read(temporary.path().join("victim/new.bin"))
            .expect("replacement canonical payload"),
        b"new",
    );
    assert!(!temporary.path().join("victim.deleted").exists());

    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn tree_removal_obligation_finishes_a_partially_cleared_root() {
    let temporary = crate::test_tempdir().expect("temporary root");
    create_test_directories(temporary.path(), &["victim", "victim/nested"]);
    std::fs::write(temporary.path().join("victim/removed.bin"), b"removed").expect("first payload");
    std::fs::write(
        temporary.path().join("victim/nested/retained.bin"),
        b"retained",
    )
    .expect("second payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_directory(&root, "victim", "victim.deleted");
    std::fs::remove_file(temporary.path().join("victim.deleted/removed.bin"))
        .expect("simulate partial tree removal");
    let obligation = DirectoryTreeRemovalObligation {
        error: io::Error::other("test tree removal stopped after partial progress"),
        parked: Some(parked),
    };

    assert!(matches!(
        obligation.reconcile(),
        DirectoryTreeRemovalResolution::Removed
    ));
    assert!(!temporary.path().join("victim.deleted").exists());

    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn parked_tree_destination_case_equivalent_collision_is_no_effect() {
    let temporary = crate::test_tempdir().expect("temporary root");
    create_test_directories(temporary.path(), &["victim", "victim/nested"]);
    std::fs::create_dir(temporary.path().join("VICTIM.DELETED"))
        .expect("case-equivalent collision");
    std::fs::write(
        temporary.path().join("VICTIM.DELETED/replacement.bin"),
        b"replacement",
    )
    .expect("collision payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let directory = root
        .open_directory(&LeafName::new("victim").expect("victim leaf"))
        .expect("victim capability");
    let park_name = LeafName::new("victim.deleted").expect("park leaf");
    assert!(leaf_names_equivalent(
        park_name.as_os_str(),
        OsStr::new("VICTIM.DELETED"),
    ));

    let directory = match directory.park_as(park_name) {
        DirectoryParkOutcome::NoEffect {
            item: directory, ..
        } => directory,
        outcome => panic!("case-equivalent destination was not preserved: {outcome:?}"),
    };
    assert_eq!(
        std::fs::read(temporary.path().join("VICTIM.DELETED/replacement.bin"))
            .expect("collision payload"),
        b"replacement",
    );
    directory
        .entries(1)
        .expect("source directory remains admitted");

    drop((directory, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn parked_tree_removal_unlinks_links_without_following_them() {
    use std::os::unix::fs::symlink;

    let temporary = crate::test_tempdir().expect("temporary root");
    let external = crate::test_tempdir().expect("external directory");
    std::fs::write(external.path().join("sentinel.bin"), b"external").expect("external sentinel");
    create_test_directories(temporary.path(), &["victim", "victim/nested"]);
    symlink(
        external.path(),
        temporary.path().join("victim/nested/external"),
    )
    .expect("external directory link");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_directory(&root, "victim", "victim.deleted");

    assert!(matches!(
        parked.remove_tree(),
        DirectoryTreeRemovalOutcome::Removed
    ));
    assert_eq!(
        std::fs::read(external.path().join("sentinel.bin")).expect("external sentinel"),
        b"external",
    );

    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn parked_tree_removal_preserves_a_replacement_root_binding() {
    let temporary = crate::test_tempdir().expect("temporary root");
    create_test_directories(temporary.path(), &["victim", "victim/nested"]);
    std::fs::write(
        temporary.path().join("victim/nested/original.bin"),
        b"original",
    )
    .expect("original payload");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_test_directory(&root, "victim", "victim.deleted");
    std::fs::rename(
        temporary.path().join("victim.deleted"),
        temporary.path().join("victim.moved"),
    )
    .expect("move parked tree out of its binding");
    std::fs::create_dir(temporary.path().join("victim.deleted")).expect("replacement root");
    std::fs::write(
        temporary.path().join("victim.deleted/replacement.bin"),
        b"replacement",
    )
    .expect("replacement payload");

    let obligation = match parked.remove_tree() {
        DirectoryTreeRemovalOutcome::Indeterminate(obligation) => obligation,
        outcome => panic!("replacement root was not retained as uncertain: {outcome:?}"),
    };
    assert_eq!(
        std::fs::read(temporary.path().join("victim.deleted/replacement.bin"))
            .expect("replacement payload"),
        b"replacement",
    );
    assert_eq!(
        std::fs::read(temporary.path().join("victim.moved/nested/original.bin"))
            .expect("original payload"),
        b"original",
    );

    std::fs::remove_file(temporary.path().join("victim.deleted/replacement.bin"))
        .expect("remove replacement payload");
    std::fs::remove_dir(temporary.path().join("victim.deleted")).expect("remove replacement root");
    std::fs::rename(
        temporary.path().join("victim.moved"),
        temporary.path().join("victim.deleted"),
    )
    .expect("restore parked tree binding");
    assert!(matches!(
        obligation.reconcile(),
        DirectoryTreeRemovalResolution::Removed
    ));
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn bounded_tree_removal_can_be_retained_and_retried() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let mut nested = temporary.path().join("victim");
    std::fs::create_dir(&nested).expect("tree root");
    for _ in 0..=platform::MAX_TREE_CLEAR_DEPTH {
        nested.push("d");
        std::fs::create_dir(&nested).expect("nested bounded directory");
    }
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let owner = root.create_effect_owner().expect("effect owner");
    let parked = park_test_directory(&root, "victim", "victim.deleted");
    let obligation = match parked.remove_tree() {
        DirectoryTreeRemovalOutcome::Indeterminate(obligation) => obligation,
        outcome => panic!("over-depth tree was not retained as uncertain: {outcome:?}"),
    };
    assert!(temporary.path().join("victim.deleted").exists());
    owner
        .retain_directory_tree_removal(obligation)
        .expect("retain bounded tree obligation");

    drop((owner, root));
    let refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("unresolved tree removal did not refuse drain: {outcome:?}"),
    };
    assert!(temporary.path().join("victim.deleted").exists());

    let deepest = (0..=platform::MAX_TREE_CLEAR_DEPTH)
        .fold(temporary.path().join("victim.deleted"), |path, _| {
            path.join("d")
        });
    std::fs::remove_dir(&deepest).expect("trim over-depth tree");
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    assert!(!temporary.path().join("victim.deleted").exists());
}

#[test]
fn raw_applied_directory_create_refuses_drain_until_reconciled() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let authority = session.authority.clone();
    let name = LeafName::new("created").expect("created leaf");
    let operation = authority.enter().expect("directory create operation");
    let token = authority
        .reserve_directory_create(&operation, &root, &name)
        .expect("directory create reservation");
    let created = match platform::create_directory(root.inner.handle(), name.as_os_str()) {
        Ok(created) => created,
        Err(_) => panic!("native directory creation failed"),
    };
    authority.attach_directory_create(&token, created);
    let obligation = DirectoryCreateObligation {
        error: io::Error::other("test directory create requires settlement"),
        token,
    };
    drop((operation, root));

    let refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("raw directory create did not refuse revocation: {outcome:?}"),
    };
    assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
    let directory = match obligation.reconcile() {
        DirectoryCreateResolution::Created(directory) => directory,
        DirectoryCreateResolution::Indeterminate(_) => {
            panic!("raw directory create remained indeterminate")
        }
    };
    drop((directory, authority));
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
}

#[test]
fn raw_live_stage_refuses_drain_and_remains_discardable() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let mut staged = match root.create_stage() {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => panic!("stage creation failed: {error}"),
        FileCreateOutcome::AppliedUnverified(obligation) => {
            panic!("stage creation was not verified: {}", obligation.error())
        }
    };
    staged.write_all(b"pending").expect("stage bytes");
    drop(root);

    let refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("raw live stage did not refuse revocation: {outcome:?}"),
    };
    assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
}

#[test]
fn dropped_owner_preserves_unclassified_directory_create_at_drain() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let owner = root.create_effect_owner().expect("effect owner");
    let authority = session.authority.clone();
    let name = LeafName::new("preserved-directory").expect("preserved leaf");
    let operation = authority.enter().expect("directory create operation");
    let token = authority
        .reserve_directory_create(&operation, &root, &name)
        .expect("directory create reservation");
    let created = match platform::create_directory(root.inner.handle(), name.as_os_str()) {
        Ok(created) => created,
        Err(_) => panic!("native directory creation failed"),
    };
    authority.attach_directory_create(&token, created);
    authority.mark_directory_create_unclassified(&token);
    owner
        .retain_directory_create_preservation(DirectoryCreatePreservation { token })
        .expect("retain directory preservation");
    drop((operation, authority, owner, root));

    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(temporary.path().join("preserved-directory").is_dir());
}

#[test]
fn effect_owner_settlement_is_fifo_and_stops_at_first_unresolved_move() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    std::fs::write(temporary.path().join("domain/first.bin"), b"first").expect("first file");
    std::fs::write(temporary.path().join("domain/second.bin"), b"second").expect("second file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");
    let first_file = domain
        .open_file(&LeafName::new("first.bin").expect("first leaf"))
        .expect("first capability");
    let second_file = domain
        .open_file(&LeafName::new("second.bin").expect("second leaf"))
        .expect("second capability");
    let first = owner
        .retain_file_move(test_file_move_obligation(
            first_file,
            domain.clone(),
            "first-target.bin",
            true,
        ))
        .expect("retain first move");
    let second = owner
        .retain_file_move(test_file_move_obligation(
            second_file,
            domain.clone(),
            "second-target.bin",
            false,
        ))
        .expect("retain second move");

    owner.settle().expect("first FIFO settlement");
    {
        let records = owner.state.effects.lock().expect("owner records");
        assert!(matches!(
            records.effects.get(&first.id),
            Some(OwnedEffect::FileMove(OwnedReceipt {
                state: Owned::Pending(_),
                ..
            }))
        ));
        assert!(matches!(
            records.effects.get(&second.id),
            Some(OwnedEffect::FileMove(OwnedReceipt {
                state: Owned::Pending(_),
                ..
            }))
        ));
    }
    {
        let mut records = owner.state.effects.lock().expect("owner records");
        let Some(OwnedEffect::FileMove(OwnedReceipt {
            state: Owned::Pending(obligation),
            ..
        })) = records.effects.get_mut(&first.id)
        else {
            panic!("first move remains pending")
        };
        obligation.reported_success = false;
    }
    owner.settle().expect("second FIFO settlement");
    let first_file = claim_no_effect(first);
    let second_file = claim_no_effect(second);
    assert!(owner.require_settled().is_ok());

    drop((first_file, second_file, owner, domain, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn promotion_replace_and_directory_move_receipts_preserve_linear_outcomes() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("movable")).expect("movable directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let owner = root.create_effect_owner().expect("effect owner");

    let promoted_stage = test_sealed_stage(&root, b"promotion");
    let promotion_name = LeafName::new("promotion.bin").expect("promotion leaf");
    let (promotion_attempt_id, promotion_attempt) =
        test_publication_attempt(&promoted_stage, &root, &promotion_name);
    let promotion = FilePromotionObligation {
        error: io::Error::other("test promotion requires settlement"),
        retained: promoted_stage,
        destination: root.clone(),
        destination_name: promotion_name,
        attempt_id: promotion_attempt_id,
        receipt: promotion_attempt,
    };
    let promotion = owner
        .retain_file_promotion(Box::new(promotion))
        .expect("retain promotion");
    owner.settle().expect("settle promotion");
    let staged = match promotion.claim() {
        FilePromotionReceiptOutcome::NoEffect(staged) => staged,
        FilePromotionReceiptOutcome::Applied(_) => {
            panic!("test promotion unexpectedly applied")
        }
        FilePromotionReceiptOutcome::Pending(_) => {
            panic!("test promotion remained pending")
        }
    };
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));

    let replacement_stage = test_sealed_stage(&root, b"replacement");
    let replacement_name = LeafName::new("replacement.bin").expect("replacement leaf");
    let (replacement_attempt_id, replacement_attempt) =
        test_publication_attempt(&replacement_stage, &root, &replacement_name);
    let replacement_promotion = FilePromotionObligation {
        error: io::Error::other("test replacement promotion requires settlement"),
        retained: replacement_stage,
        destination: root.clone(),
        destination_name: replacement_name.clone(),
        attempt_id: replacement_attempt_id,
        receipt: replacement_attempt,
    };
    let replacement = owner
        .retain_file_replace(FileReplaceObligation {
            error: io::Error::other("test replacement requires settlement"),
            state: Some(Box::new(FileReplaceObligationState::Promoting {
                promotion: Box::new(replacement_promotion),
                displaced: None,
                fallback: ReplaceDestination::Vacant {
                    parent: root.clone(),
                    name: replacement_name,
                },
                receipt: None,
            })),
        })
        .expect("retain replacement");
    drop(replacement);
    owner.settle().expect("reclaim dropped replacement result");

    let movable = root
        .open_directory(&LeafName::new("movable").expect("movable leaf"))
        .expect("movable capability");
    let directory_move = owner
        .retain_directory_move(test_directory_move_obligation(
            movable,
            root.clone(),
            "moved",
            false,
        ))
        .expect("retain directory move");
    owner.settle().expect("settle directory move");
    let movable = match directory_move.claim() {
        DirectoryMoveReceiptOutcome::NoEffect(directory) => directory,
        DirectoryMoveReceiptOutcome::Applied(_) => {
            panic!("test directory move unexpectedly applied")
        }
        DirectoryMoveReceiptOutcome::Pending(_) => {
            panic!("test directory move remained pending")
        }
    };
    drop((movable, owner, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn retained_replacement_restores_during_terminal_settlement() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("destination.bin"), b"old").expect("destination file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let owner = root.create_effect_owner().expect("effect owner");
    let destination = root
        .open_file(&LeafName::new("destination.bin").expect("destination leaf"))
        .expect("destination capability");
    let revision = destination.revision().expect("destination revision");
    let digest: [u8; 32] = Sha256::digest(b"old").into();
    let request = destination.park_request(ExpectedFileContent::new(revision, digest));
    let expected = ExpectedContentReceipt::capture(&request);
    let parked = match root.park_file_as(
        request,
        LeafName::new("destination.parked").expect("park leaf"),
    ) {
        FileParkOutcome::Parked(parked) => parked,
        FileParkOutcome::NoEffect { error, .. } => {
            panic!("destination park had no effect: {error}")
        }
        FileParkOutcome::Preserved { error, .. } => {
            panic!("destination park retained changed content: {error}")
        }
        FileParkOutcome::AppliedUnverified(obligation) => {
            panic!("destination park was not verified: {}", obligation.error())
        }
    };
    let staged = test_sealed_stage(&root, b"new");
    let receipt = owner
        .retain_file_replace(FileReplaceObligation {
            error: io::Error::other("test replacement rollback requires settlement"),
            state: Some(Box::new(FileReplaceObligationState::RestoreParked {
                parked,
                staged,
                receipt: expected,
            })),
        })
        .expect("retain replacement rollback");
    drop((owner, root));

    let refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("live replacement receipt did not block revocation: {outcome:?}"),
    };
    let (staged, destination) = match receipt.claim() {
        FileReplaceReceiptOutcome::NoEffect(refused) => refused,
        FileReplaceReceiptOutcome::Applied(_) => {
            panic!("test replacement unexpectedly applied")
        }
        FileReplaceReceiptOutcome::Pending(_) => {
            panic!("test replacement remained pending")
        }
    };
    let request = match destination {
        ReplaceDestination::Existing(request) => request,
        ReplaceDestination::Vacant { .. } => {
            panic!("restored replacement destination became vacant")
        }
        ReplaceDestination::Preserved(_) => {
            panic!("unchanged restored replacement lost retry authority")
        }
    };
    let (file, revision, _) = request.into_parts();
    file.validate_revision(&revision)
        .expect("restored replacement request carries the refreshed revision");
    drop(file);
    assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    assert_eq!(
        std::fs::read(temporary.path().join("destination.bin")).expect("restored destination"),
        b"old",
    );
}

#[test]
fn effect_owner_counts_are_bounded_and_dead_handles_release_capacity() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let mut owners = (0..MAX_EFFECT_OWNERS)
        .map(|_| domain.create_effect_owner().expect("bounded effect owner"))
        .collect::<Vec<_>>();
    let error = domain
        .create_effect_owner()
        .expect_err("owner capacity must apply backpressure");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    owners.pop();
    owners.push(
        domain
            .create_effect_owner()
            .expect("dead owner handle releases capacity"),
    );

    drop((owners, domain, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn effect_and_terminal_result_counts_share_one_bounded_capacity() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
    for index in 0..MAX_EFFECTS_PER_OWNER {
        std::fs::write(
            temporary
                .path()
                .join("domain")
                .join(format!("source-{index}.bin")),
            b"source",
        )
        .expect("unique source file");
    }
    std::fs::write(
        temporary.path().join("domain/overflow-source.bin"),
        b"overflow source",
    )
    .expect("overflow source file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let domain = root
        .open_directory(&LeafName::new("domain").expect("domain leaf"))
        .expect("domain capability");
    let owner = domain.create_effect_owner().expect("effect owner");
    let mut receipts = Vec::with_capacity(MAX_EFFECTS_PER_OWNER);
    for index in 0..MAX_EFFECTS_PER_OWNER {
        let source_name = LeafName::new(format!("source-{index}.bin")).expect("unique source leaf");
        let file = domain.open_file(&source_name).expect("source capability");
        receipts.push(
            owner
                .retain_file_move(test_file_move_obligation(
                    file,
                    domain.clone(),
                    &format!("target-{index}.bin"),
                    false,
                ))
                .expect("bounded retained move"),
        );
    }
    let overflow_file = domain
        .open_file(&LeafName::new("overflow-source.bin").expect("overflow source leaf"))
        .expect("overflow source capability");
    let failure = owner
        .retain_file_move(test_file_move_obligation(
            overflow_file,
            domain.clone(),
            "overflow-target.bin",
            false,
        ))
        .expect_err("effect capacity must apply backpressure");
    let (error, mut overflow) = failure.into_parts();
    let error_kind = error.kind();
    let overflow_file = overflow.file.take().expect("returned overflow file");
    let authority = overflow_file
        .parent
        .authority()
        .expect("overflow authority");
    let operation = authority.enter().expect("overflow settlement operation");
    overflow
        .token
        .settle(&operation)
        .expect("settle returned overflow token");
    drop((operation, overflow_file, overflow));

    owner.settle().expect("settle bounded effects");
    assert!(owner.require_settled().is_err());
    for receipt in receipts {
        drop(claim_no_effect(receipt));
    }
    assert!(owner.require_settled().is_ok());
    assert_eq!(error_kind, io::ErrorKind::WouldBlock);

    drop((owner, domain, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn leaf_names_are_exact_single_components() {
    for value in ["", ".", "..", "nested/name"] {
        assert!(LeafName::new(value).is_err());
    }
    assert!(LeafName::new("state.json").is_ok());
}

#[test]
fn a_second_root_session_fails_without_waiting() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
    ));
    drop(first);
    drop(acquire_test_root(temporary.path()));
}

#[test]
fn file_capabilities_compare_private_physical_identity() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("first.bin"), b"first").expect("first file");
    std::fs::write(temporary.path().join("second.bin"), b"second").expect("second file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let first = root
        .open_file(&LeafName::new("first.bin").expect("first leaf"))
        .expect("first capability");
    let first_again = root
        .open_file(&LeafName::new("first.bin").expect("first leaf"))
        .expect("second first capability");
    let second = root
        .open_file(&LeafName::new("second.bin").expect("second leaf"))
        .expect("second capability");

    assert!(first.could_be_same_file(&first_again));
    assert!(!first.could_be_same_file(&second));
    assert!(first.same_file(&first_again).expect("same-file proof"));
    assert!(!first.same_file(&second).expect("distinct-file proof"));

    std::fs::hard_link(
        temporary.path().join("first.bin"),
        temporary.path().join("alias.bin"),
    )
    .expect("hard-link original");
    assert!(first.could_be_same_file(&first_again));
    assert!(first.same_file(&first_again).is_err());
    assert!(first.same_file(&second).is_err());
    drop((root, first, first_again, second));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_revision_witness_survives_root_reopen_and_fixed_width_encoding() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("file");
    let leaf = LeafName::new("record.bin").expect("leaf");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root.open_file(&leaf).expect("file capability");
    let witness = file.revision_witness().expect("revision witness");
    let encoded = hex::encode(witness);
    assert_eq!(encoded.len(), 64);
    let mut decoded = [0; 32];
    hex::decode_to_slice(&encoded, &mut decoded).expect("fixed-width witness");
    assert_eq!(decoded, witness);
    assert!(hex::decode_to_slice(&encoded[..62], &mut decoded).is_err());
    drop((file, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));

    let reopened = acquire_test_root(temporary.path());
    let root = reopened.root().expect("reopened root");
    let file = root.open_file(&leaf).expect("reopened file");
    assert_eq!(file.revision_witness().expect("reopened witness"), witness);
    drop((file, root));
    assert!(matches!(reopened.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_revision_witness_refuses_same_bytes_replacement_and_stale_binding() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let path = temporary.path().join("record.bin");
    std::fs::write(&path, b"payload").expect("file");
    let leaf = LeafName::new("record.bin").expect("leaf");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root.open_file(&leaf).expect("file capability");
    let witness = file.revision_witness().expect("revision witness");
    #[cfg(windows)]
    drop(file);

    std::fs::rename(&path, temporary.path().join("original.bin")).expect("retain original inode");
    std::fs::write(&path, b"payload").expect("same bytes replacement");
    #[cfg(unix)]
    assert!(file.revision_witness().is_err());
    let replacement = root.open_file(&leaf).expect("replacement capability");
    assert_ne!(
        replacement.revision_witness().expect("replacement witness"),
        witness
    );
    #[cfg(unix)]
    drop(file);
    drop((replacement, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_revision_witness_refuses_in_place_size_and_equal_bytes_revision_changes() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let path = temporary.path().join("record.bin");
    std::fs::write(&path, b"payload").expect("file");
    let writer = File::options().write(true).open(&path).expect("writer");
    let original_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(86_400);
    writer
        .set_times(std::fs::FileTimes::new().set_modified(original_time))
        .expect("initial revision timestamp");
    drop(writer);
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let leaf = LeafName::new("record.bin").expect("leaf");
    let file = root.open_file(&leaf).expect("file capability");
    let witness = file.revision_witness().expect("revision witness");
    let identity = file.identity;
    drop(file);

    std::fs::write(&path, b"payload").expect("equal bytes in-place write");
    let writer = File::options().write(true).open(&path).expect("writer");
    writer
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(original_time + std::time::Duration::from_secs(86_400)),
        )
        .expect("distinct revision timestamp");
    drop(writer);
    let readmitted = root.open_file(&leaf).expect("same physical file");
    assert!(readmitted.identity == identity);
    let changed = readmitted.revision_witness().expect("changed witness");
    assert_ne!(changed, witness);
    drop(readmitted);

    let writer = File::options().write(true).open(&path).expect("writer");
    writer.set_len(1).expect("in-place size change");
    drop(writer);
    let file = root.open_file(&leaf).expect("resized file");
    assert_ne!(file.revision_witness().expect("resized witness"), changed);
    drop((file, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_revision_witness_refuses_unrepresentable_metadata() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let path = temporary.path().join("record.bin");
    std::fs::write(&path, b"payload").expect("file");
    let writer = File::options().write(true).open(&path).expect("writer");
    writer
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)),
        )
        .expect("pre-epoch timestamp");
    drop(writer);
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root
        .open_file(&LeafName::new("record.bin").expect("leaf"))
        .expect("file capability");
    assert!(file.revision().expect("revision").modified_at_ns().is_err());
    assert_eq!(
        file.revision_witness()
            .expect_err("unsupported timestamp must not lose revision evidence")
            .kind(),
        io::ErrorKind::Unsupported
    );
    drop((file, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_revision_witness_refuses_revoked_authority() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root
        .open_file(&LeafName::new("record.bin").expect("leaf"))
        .expect("file capability");
    file.revision_witness().expect("live witness");
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        file.revision_witness()
            .expect_err("revoked authority must refuse")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn directory_incarnation_witness_survives_child_cleanup_and_root_reopen() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let path = temporary.path().join("staged");
    std::fs::create_dir(&path).expect("staged directory");
    std::fs::write(path.join("payload.bin"), b"payload").expect("staged payload");
    let leaf = LeafName::new("staged").expect("leaf");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let directory = root.open_directory(&leaf).expect("directory capability");
    let witness = match directory.incarnation_witness() {
        Ok(witness) => witness,
        Err(error) => {
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            eprintln!("filesystem birth time unavailable; no cleanup-stability proof exercised");
            drop((directory, root));
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            return;
        }
    };
    std::fs::remove_file(path.join("payload.bin")).expect("remove staged payload");
    assert_eq!(
        directory.incarnation_witness().expect("cleaned witness"),
        witness
    );
    drop((directory, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));

    let reopened = acquire_test_root(temporary.path());
    let root = reopened.root().expect("reopened root");
    let directory = root.open_directory(&leaf).expect("reopened directory");
    assert_eq!(
        directory.incarnation_witness().expect("reopened witness"),
        witness
    );
    drop((directory, root));
    assert!(matches!(reopened.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_incarnation_witness_refuses_replacement_and_stale_binding() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let path = temporary.path().join("staged");
    std::fs::create_dir(&path).expect("staged directory");
    let leaf = LeafName::new("staged").expect("leaf");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let directory = root.open_directory(&leaf).expect("directory capability");
    let witness = match directory.incarnation_witness() {
        Ok(witness) => witness,
        Err(error) => {
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            eprintln!("filesystem birth time unavailable; no replacement proof exercised");
            drop((directory, root));
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            return;
        }
    };
    std::fs::rename(&path, temporary.path().join("original")).expect("retain original directory");
    std::fs::create_dir(&path).expect("replacement directory");
    assert!(directory.incarnation_witness().is_err());
    let replacement = root.open_directory(&leaf).expect("replacement capability");
    assert_ne!(
        replacement
            .incarnation_witness()
            .expect("replacement witness"),
        witness
    );
    drop((replacement, directory, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_incarnation_witness_refuses_unavailable_birth_metadata() {
    for (seconds, nanos) in [(0, 0), (-1, 0), (1, -1), (1, 1_000_000_000), (i64::MAX, 0)] {
        assert_eq!(
            platform::birth_time_ns(seconds, nanos)
                .expect_err("unrepresentable birth metadata must not become an identity-only proof")
                .kind(),
            io::ErrorKind::Unsupported
        );
    }
    assert_eq!(
        platform::birth_time_ns(1, 23).expect("exact timestamp"),
        1_000_000_023
    );
}

#[test]
fn directory_incarnation_witness_refuses_revoked_authority() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        root.incarnation_witness()
            .expect_err("revoked authority must refuse before metadata inspection")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn move_topology_never_reclassifies_reported_success_as_no_effect() {
    assert_eq!(
        classify_move_topology(
            true,
            Some(platform::BindingState::Exact),
            Some(platform::BindingState::Absent),
        ),
        MoveTopology::Indeterminate,
    );
    assert_eq!(
        classify_move_topology(
            false,
            Some(platform::BindingState::Exact),
            Some(platform::BindingState::Absent),
        ),
        MoveTopology::NoEffect,
    );
    assert_eq!(
        classify_move_topology(
            true,
            Some(platform::BindingState::Absent),
            Some(platform::BindingState::Exact),
        ),
        MoveTopology::Applied,
    );
}

#[test]
fn unsettled_move_is_valid_pending_state_and_settlement_restores_drainability() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let authority = session.authority.clone();
    let mut token = {
        let operation = authority.enter().expect("move reservation operation");
        MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: session.root().expect("source parent"),
                name: LeafName::new("source.bin").expect("source leaf"),
            },
            NamespaceLeaf {
                parent: session.root().expect("destination parent"),
                name: LeafName::new("destination.bin").expect("destination leaf"),
            },
            None,
            None,
        )
        .expect("move effect reservation")
    };
    {
        let state = authority.operations.lock().expect("operation state");
        assert_eq!(state.outstanding_effects, 1);
        assert_eq!(state.moves.len(), 1);
        assert!(state.moves.contains_key(&token.id));
        assert_eq!(state.phase, AUTHORITY_LIVE);
    }
    let error = authority
        .begin_terminal_drain(false)
        .expect_err("unsettled move blocks terminal drain");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(
        authority.operations.lock().expect("operation state").phase,
        AUTHORITY_LIVE,
    );
    {
        let operation = authority.enter().expect("move settlement operation");
        token.settle(&operation).expect("settle move effect");
    }
    {
        let state = authority.operations.lock().expect("operation state");
        assert_eq!(state.outstanding_effects, 0);
        assert!(state.moves.is_empty());
    }
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn file_move_is_no_replace_and_collision_is_no_effect() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("source")).expect("source directory");
    std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
    std::fs::write(temporary.path().join("source/moved.bin"), b"moved").expect("moved source");
    std::fs::write(temporary.path().join("source/collision.bin"), b"source")
        .expect("collision source");
    std::fs::write(
        temporary.path().join("destination/collision.bin"),
        b"destination",
    )
    .expect("collision destination");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let source = root
        .open_directory(&LeafName::new("source").expect("source leaf"))
        .expect("source capability");
    let destination = root
        .open_directory(&LeafName::new("destination").expect("destination leaf"))
        .expect("destination capability");

    let moved = source
        .open_file(&LeafName::new("moved.bin").expect("moved leaf"))
        .expect("moved capability");
    let moved = match moved.move_no_replace(
        &destination,
        &LeafName::new("published.bin").expect("published leaf"),
    ) {
        FileMoveOutcome::Applied(file) => file,
        FileMoveOutcome::NoEffect { error, .. } => panic!("move had no effect: {error}"),
        FileMoveOutcome::AppliedUnverified(obligation) => {
            panic!("move was indeterminate: {}", obligation.error())
        }
    };
    assert_eq!(moved.read_bounded(16).expect("moved bytes"), b"moved");

    let collision = source
        .open_file(&LeafName::new("collision.bin").expect("collision leaf"))
        .expect("collision source capability");
    match collision.move_no_replace(
        &destination,
        &LeafName::new("collision.bin").expect("collision leaf"),
    ) {
        FileMoveOutcome::NoEffect { item: file, .. } => {
            assert_eq!(file.read_bounded(16).expect("source bytes"), b"source");
        }
        FileMoveOutcome::Applied(_) => panic!("collision replaced its destination"),
        FileMoveOutcome::AppliedUnverified(obligation) => {
            panic!("collision was indeterminate: {}", obligation.error())
        }
    }
    assert_eq!(
        std::fs::read(temporary.path().join("destination/collision.bin"))
            .expect("destination bytes"),
        b"destination"
    );
    drop((root, source, destination, moved));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn file_move_parent_barrier_failures_retain_receipt_until_reconciled() {
    for fail_source in [false, true] {
        let temporary = crate::test_tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("source")).unwrap();
        std::fs::create_dir(temporary.path().join("destination")).unwrap();
        std::fs::write(temporary.path().join("source/payload.bin"), b"payload").unwrap();
        let session = acquire_test_root(temporary.path());
        let root = session.root().unwrap();
        let source = root
            .open_directory(&LeafName::new("source").unwrap())
            .unwrap();
        let destination = root
            .open_directory(&LeafName::new("destination").unwrap())
            .unwrap();
        let owner = root.create_effect_owner().unwrap();
        let file = source
            .open_file(&LeafName::new("payload.bin").unwrap())
            .unwrap();
        let original_identity = file.identity;
        let failures = if fail_source {
            vec![Ok(()), Err(io::ErrorKind::Other)]
        } else {
            vec![Err(io::ErrorKind::Other)]
        };
        let obligation = {
            let _fault =
                platform::install_publication_directory_sync_test_outcomes(failures.clone());
            match file.move_no_replace(&destination, &LeafName::new("published.bin").unwrap()) {
                FileMoveOutcome::AppliedUnverified(obligation) => obligation,
                outcome => panic!("failed parent barrier became terminal: {outcome:?}"),
            }
        };
        assert!(!temporary.path().join("source/payload.bin").exists());
        assert_eq!(
            std::fs::read(temporary.path().join("destination/published.bin")).unwrap(),
            b"payload"
        );
        let receipt = owner.retain_file_move(obligation).unwrap();
        {
            let _fault = platform::install_publication_directory_sync_test_outcomes(failures);
            owner.settle().unwrap();
        }
        let receipt = match receipt.claim() {
            FileMoveReceiptOutcome::Pending(receipt) => receipt,
            outcome => panic!("repeated parent barrier failure became terminal: {outcome:?}"),
        };
        assert!(owner.require_settled().is_err());

        owner.settle().unwrap();
        let moved = match receipt.claim() {
            FileMoveReceiptOutcome::Applied(file) => file,
            outcome => panic!("successful parent barriers did not settle: {outcome:?}"),
        };
        assert!(moved.identity == original_identity);
        assert_eq!(moved.read_bounded(7).unwrap(), b"payload");
        assert!(!temporary.path().join("source/payload.bin").exists());
        assert!(owner.require_settled().is_ok());
        drop((moved, owner, source, destination, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }
}

#[test]
fn directory_move_is_no_replace_and_collision_is_no_effect() {
    let temporary = crate::test_tempdir().expect("temporary root");
    for relative in [
        "source",
        "destination",
        "source/moved",
        "source/collision",
        "destination/collision",
    ] {
        std::fs::create_dir(temporary.path().join(relative)).expect("test directory");
    }
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let source = root
        .open_directory(&LeafName::new("source").expect("source leaf"))
        .expect("source capability");
    let destination = root
        .open_directory(&LeafName::new("destination").expect("destination leaf"))
        .expect("destination capability");

    let moved = source
        .open_directory(&LeafName::new("moved").expect("moved leaf"))
        .expect("moved capability");
    let moved = match moved.move_no_replace(
        &destination,
        &LeafName::new("published").expect("published leaf"),
    ) {
        DirectoryMoveOutcome::Applied(directory) => directory,
        DirectoryMoveOutcome::NoEffect { error, .. } => {
            panic!("directory move had no effect: {error}")
        }
        DirectoryMoveOutcome::AppliedUnverified(obligation) => {
            panic!("directory move was indeterminate: {}", obligation.error())
        }
    };
    moved.entries(1).expect("moved directory remains admitted");

    let collision = source
        .open_directory(&LeafName::new("collision").expect("collision leaf"))
        .expect("collision source capability");
    match collision.move_no_replace(
        &destination,
        &LeafName::new("collision").expect("collision leaf"),
    ) {
        DirectoryMoveOutcome::NoEffect {
            item: directory, ..
        } => {
            directory.entries(1).expect("source remains admitted");
        }
        DirectoryMoveOutcome::Applied(_) => panic!("collision replaced its destination"),
        DirectoryMoveOutcome::AppliedUnverified(obligation) => {
            panic!("collision was indeterminate: {}", obligation.error())
        }
    }
    assert!(temporary.path().join("destination/collision").is_dir());
    drop((root, source, destination, moved));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn root_clear_retains_the_lease_until_release() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("owned.bin"), b"owned").expect("owned file");
    let session = acquire_test_root(temporary.path());
    let reset = match session.begin_reset() {
        ResetStartOutcome::Ready(authority) => authority,
        outcome => panic!("reset did not become ready: {outcome:?}"),
    };
    reset.try_clear_root(&[]).expect("root clear");
    assert!(!temporary.path().join("owned.bin").exists());
    assert!(temporary.path().join(ROOT_LEASE_NAME).is_file());
    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
    ));
    reset.release().expect("release reset authority");
    drop(acquire_test_root(temporary.path()));
}

#[cfg(unix)]
#[test]
fn retained_root_lease_rejects_a_new_hardlink_before_identity_read() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let canary = temporary.path().join("canary.bin");
    std::fs::write(&canary, b"preserve me").expect("canary");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let identity = root.identity().expect("original root identity");
    let alias = temporary.path().join("lease-hardlink");
    std::fs::hard_link(temporary.path().join(ROOT_LEASE_NAME), &alias)
        .expect("retained lease hardlink");
    let error = root.identity().expect_err("linked lease must refuse reads");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "filesystem capability is not an exact single-link regular file"
    );
    assert_eq!(
        std::fs::read(&canary).expect("canary after refusal"),
        b"preserve me"
    );
    std::fs::remove_file(alias).expect("remove disposable hardlink");
    assert_eq!(root.identity().expect("restored lease read"), identity);
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn root_lease_is_a_retained_single_link_file_and_serializes_sessions() {
    use std::os::unix::fs::MetadataExt;

    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let lease = temporary.path().join(ROOT_LEASE_NAME);
    let metadata = std::fs::symlink_metadata(&lease).expect("lease metadata");
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.nlink(), 1);
    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
    ));

    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(lease.is_file(), "lease file should remain reusable");
    assert!(matches!(
        acquire_test_root(temporary.path()).revoke(),
        RootRevokeOutcome::Revoked
    ));
}

#[cfg(unix)]
#[test]
fn root_lease_rejects_a_noncanonical_portable_alias_before_acquisition() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let alias = temporary.path().join(".AXIAL-ROOT.LEASE");
    std::fs::write(&alias, b"user-owned alias").expect("portable lease alias");

    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(_)
    ));
    assert!(alias.is_file());
    assert!(
        std::fs::read_dir(temporary.path())
            .expect("list root after lease alias refusal")
            .all(|entry| entry.expect("read root entry").file_name() != ROOT_LEASE_NAME)
    );
}

#[cfg(windows)]
#[test]
fn windows_root_lease_accepts_exact_and_rejects_noncanonical_spelling() {
    let exact = crate::test_tempdir().expect("exact temporary root");
    File::create(exact.path().join(ROOT_LEASE_NAME))
        .expect("create exact lease")
        .set_len(recovery::RECOVERY_CONTROL_BYTES)
        .expect("size exact lease");
    assert!(matches!(
        acquire_test_root(exact.path()).revoke(),
        RootRevokeOutcome::Revoked
    ));

    let noncanonical = crate::test_tempdir().expect("noncanonical temporary root");
    let alias = noncanonical.path().join(".AXIAL-ROOT.LEASE");
    std::fs::write(&alias, b"user-owned alias").expect("noncanonical lease alias");
    assert!(matches!(
        RootSession::acquire(noncanonical.path()),
        RootSessionAcquireOutcome::NoEffect(_)
    ));
    let names = std::fs::read_dir(noncanonical.path())
        .expect("enumerate noncanonical root")
        .map(|entry| entry.expect("root entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, [OsString::from(".AXIAL-ROOT.LEASE")]);
}

#[cfg(target_os = "linux")]
#[test]
fn retained_root_lease_rejects_a_new_portable_alias_before_control_io() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let alias = temporary.path().join(".AXIAL-ROOT.LEASE");
    std::fs::write(&alias, b"user-owned alias").expect("portable lease alias");

    let mut byte = [0; 1];
    assert!(
        platform::recovery_control_read_exact_at(&session.authority.lease, 0, &mut byte).is_err()
    );
    std::fs::remove_file(alias).expect("remove portable lease alias");
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn recovery_control_refuses_a_displaced_root_and_replacement_lease() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let root = temporary.path().join("app");
    let displaced = temporary.path().join("displaced-app");
    std::fs::create_dir(&root).expect("application root");
    let first = acquire_test_root(&root);
    platform::recovery_control_initialize_len(&first.authority.lease)
        .expect("initialize recovery control");
    platform::recovery_control_write_all_at(&first.authority.lease, 0, &[0x11])
        .expect("write recovery control");
    platform::recovery_control_sync(&first.authority.lease).expect("sync recovery control");

    std::fs::rename(&root, &displaced).expect("displace application root");
    std::fs::create_dir(&root).expect("replacement application root");
    let replacement = acquire_test_root(&root);
    assert!(platform::recovery_control_write_all_at(&first.authority.lease, 0, &[0x22]).is_err());
    assert!(first.validate_retained_authority().is_err());

    let mut displaced_control =
        File::open(displaced.join(ROOT_LEASE_NAME)).expect("open displaced recovery control");
    let mut first_byte = [0_u8; 1];
    displaced_control
        .read_exact(&mut first_byte)
        .expect("read displaced recovery control");
    assert_eq!(first_byte, [0x11]);

    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));
    assert!(matches!(replacement.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn windows_exclusive_recovery_control_prevents_root_displacement() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let root = temporary.path().join("app");
    let displaced = temporary.path().join("displaced-app");
    std::fs::create_dir(&root).expect("application root");
    let session = acquire_test_root(&root);

    std::fs::rename(&root, &displaced)
        .expect_err("exclusive recovery control must prevent live root displacement");
    std::fs::create_dir(&root).expect_err("occupied live root must prevent replacement creation");
    assert!(root.is_dir());
    assert!(!displaced.exists());
    platform::recovery_control_write_all_at(&session.authority.lease, 0, &[0x31])
        .expect("retained control write after denied root displacement");
    let mut byte = [0_u8; 1];
    platform::recovery_control_read_exact_at(&session.authority.lease, 0, &mut byte)
        .expect("retained control read after denied root displacement");
    assert_eq!(byte, [0x31]);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(any(unix, windows))]
#[test]
fn recovery_control_initializes_only_zero_length_and_rejects_corrupt_lengths() {
    for (length, accepted) in [
        (0, true),
        (1, false),
        (recovery::RECOVERY_CONTROL_BYTES - 1, false),
        (recovery::RECOVERY_CONTROL_BYTES, true),
        (recovery::RECOVERY_CONTROL_BYTES + 1, false),
    ] {
        let temporary = crate::test_tempdir().expect("temporary root");
        let lease_path = temporary.path().join(ROOT_LEASE_NAME);
        File::create(&lease_path)
            .expect("create lease fixture")
            .set_len(length)
            .expect("size lease fixture");
        let outcome = RootSession::acquire(temporary.path());
        let expected = if length == 0 {
            recovery::RECOVERY_CONTROL_BYTES
        } else {
            length
        };
        assert_eq!(
            std::fs::metadata(&lease_path)
                .expect("lease metadata")
                .len(),
            expected,
            "initialization changed a nonzero corrupt length"
        );
        let session = if accepted {
            match outcome {
                RootSessionAcquireOutcome::Acquired(session) => session,
                other => panic!("valid recovery control was refused: {other:?}"),
            }
        } else {
            let obligation = match outcome {
                RootSessionAcquireOutcome::AppliedUnverified(obligation)
                    if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
                {
                    obligation
                }
                other => panic!("corrupt recovery control did not retain ownership: {other:?}"),
            };
            obligation
                .acknowledge_preserved()
                .expect("release corrupt recovery control ownership");
            File::options()
                .write(true)
                .open(&lease_path)
                .expect("open corrupt recovery control")
                .set_len(recovery::RECOVERY_CONTROL_BYTES)
                .expect("repair recovery control length");
            acquire_test_root(temporary.path())
        };
        let mut last = [0xff_u8; 1];
        platform::recovery_control_read_exact_at(
            &session.authority.lease,
            recovery::RECOVERY_CONTROL_BYTES - 1,
            &mut last,
        )
        .expect("read exact recovery control tail");
        assert_eq!(last, [0]);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }
}

#[cfg(windows)]
#[test]
fn windows_exclusive_recovery_control_prevents_binding_substitution() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let lease = temporary.path().join(ROOT_LEASE_NAME);
    let displaced = temporary.path().join("displaced-control");
    std::fs::rename(&lease, &displaced)
        .expect_err("exclusive retained control must prevent displacement");
    std::fs::remove_file(&lease).expect_err("exclusive retained control must prevent removal");
    File::create(&lease).expect_err("exclusive retained control must prevent substitution");

    platform::recovery_control_write_all_at(&session.authority.lease, 0, &[0x42])
        .expect("retained control write after denied substitution");
    let mut byte = [0_u8; 1];
    platform::recovery_control_read_exact_at(&session.authority.lease, 0, &mut byte)
        .expect("retained control read after denied substitution");
    assert_eq!(byte, [0x42]);
    assert!(!displaced.exists());
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(windows)]
#[test]
fn windows_recovery_control_reports_short_native_eof() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let hook =
        platform::install_recovery_control_read_test_hook(recovery::RECOVERY_CONTROL_BYTES - 1);
    let error = platform::recovery_control_read_exact_at(
        &session.authority.lease,
        recovery::RECOVERY_CONTROL_BYTES - 1,
        &mut [0_u8; 1],
    )
    .expect_err("short positional read must fail");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    drop(hook);
    platform::set_recovery_control_len_for_test(
        &session.authority.lease,
        recovery::RECOVERY_CONTROL_BYTES,
    )
    .expect("repair retained control length");
    let mut repaired = [0xff_u8; 1];
    platform::recovery_control_read_exact_at(
        &session.authority.lease,
        recovery::RECOVERY_CONTROL_BYTES - 1,
        &mut repaired,
    )
    .expect("read repaired recovery control tail");
    assert_eq!(repaired, [0]);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn recovery_failure_preserves_root_artifacts_and_releases_the_lease() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let lease_path = temporary.path().join(ROOT_LEASE_NAME);
    let payload_path = temporary.path().join("owned.bin");
    let recovery_path = temporary
        .path()
        .join(".axial-rstage-11111111111111111111111111111111");
    std::fs::write(&lease_path, [0x7a]).expect("write corrupt recovery control");
    std::fs::write(&payload_path, b"owned payload").expect("write owned payload");
    std::fs::write(&recovery_path, b"recovery payload").expect("write recovery payload");

    let obligation = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("corrupt recovery control did not retain ownership: {outcome:?}"),
    };
    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
    ));
    #[cfg(unix)]
    let obligation = {
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        let displaced = temporary.path().join("displaced-replay-lease");
        std::fs::rename(&lease, &displaced).expect("displace retained replay lease");
        std::fs::write(&lease, b"replacement").expect("replace replay lease binding");
        let obligation = obligation
            .acknowledge_preserved()
            .expect_err("invalid replay lease must restore acquired ownership");
        std::fs::remove_file(&lease).expect("remove replacement replay lease");
        std::fs::rename(&displaced, &lease).expect("restore retained replay lease");
        obligation
    };
    let obligation = obligation
        .cleanup()
        .expect_err("recovery uncertainty must refuse destructive root cleanup");
    obligation
        .acknowledge_preserved()
        .expect("validated recovery failure can preserve its root");

    assert_eq!(std::fs::read(&lease_path).expect("read control"), [0x7a]);
    assert_eq!(
        std::fs::read(&payload_path).expect("read owned payload"),
        b"owned payload"
    );
    assert_eq!(
        std::fs::read(&recovery_path).expect("read recovery payload"),
        b"recovery payload"
    );
    let reacquired = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("preservation did not release the root lease: {outcome:?}"),
    };
    reacquired
        .acknowledge_preserved()
        .expect("settle the repeated recovery failure");
}

#[test]
fn root_recovery_control_is_reserved_only_at_the_physical_root() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let nested_path = temporary.path().join("nested");
    std::fs::create_dir(&nested_path).expect("nested directory");
    std::fs::write(nested_path.join(ROOT_LEASE_NAME), b"user-owned").expect("nested user file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let alternate = session
        .admit_absolute_directory(temporary.path())
        .expect("alternate root capability");
    let control = LeafName::new(ROOT_LEASE_NAME).expect("control leaf");
    assert_eq!(
        root.open_file(&control)
            .expect_err("root control must not become a file capability")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        alternate
            .open_file(&control)
            .expect_err("alternate root handle must not bypass control ownership")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let nested = root
        .open_directory(&LeafName::new("nested").expect("nested leaf"))
        .expect("open nested directory");
    nested
        .open_file(&control)
        .expect("same spelling below root remains user-owned");
    drop((nested, alternate, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn bounded_recovery_stage_publishes_through_the_live_lifecycle() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let target = LeafName::new("published.bin").expect("target leaf");
    let mut staged = match root.create_recoverable_stage(&target) {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => panic!("recovery create failed: {error}"),
        FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileCreateResolution::Created(staged) => staged,
            FileCreateResolution::NoEffect(error) => {
                panic!("recovery create reconciled to no effect: {error}")
            }
            FileCreateResolution::Indeterminate(obligation) => {
                panic!("recovery create remained unsettled: {}", obligation.error())
            }
        },
    };
    staged.write_all(b"durable payload").expect("write stage");
    let sealed = staged.seal().expect("seal recovery stage");
    let published = require_test_promotion(sealed.promote_no_replace(&root, &root, &target));
    assert_eq!(
        std::fs::read(temporary.path().join("published.bin")).expect("published payload"),
        b"durable payload"
    );
    drop((published, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn recoverable_state_stages_are_serialized_from_reservation_to_retirement() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let first_target = LeafName::new("first.json").expect("first target");
    let second_target = LeafName::new("second.json").expect("second target");
    let first = match root.create_recoverable_stage(&first_target) {
        FileCreateOutcome::Created(staged) => staged,
        outcome => panic!("first recovery stage was not created: {outcome:?}"),
    };
    match root.create_recoverable_stage(&second_target) {
        FileCreateOutcome::NoEffect(error) => {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock)
        }
        outcome => panic!("second recovery stage bypassed serialization: {outcome:?}"),
    }
    require_test_stage_discard(first.discard());
    let second = match root.create_recoverable_stage(&second_target) {
        FileCreateOutcome::Created(staged) => staged,
        outcome => panic!("retired recovery stage did not release serialization: {outcome:?}"),
    };
    require_test_stage_discard(second.discard());
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

fn require_test_durable_replace(mut outcome: DurableReplaceOutcome) {
    for _ in 0..8 {
        outcome = match outcome {
            DurableReplaceOutcome::Replaced => return,
            DurableReplaceOutcome::NoEffect(error) => {
                panic!("durable replacement had no effect: {error}")
            }
            DurableReplaceOutcome::AppliedUnverified(obligation) => obligation.reconcile(),
        };
    }
    panic!("durable replacement remained unsettled")
}

fn require_test_durable_replace_no_effect(mut outcome: DurableReplaceOutcome) -> io::Error {
    for _ in 0..8 {
        outcome = match outcome {
            DurableReplaceOutcome::NoEffect(error) => return error,
            DurableReplaceOutcome::Replaced => {
                panic!("durable replacement unexpectedly replaced its destination")
            }
            DurableReplaceOutcome::AppliedUnverified(obligation) => obligation.reconcile(),
        };
    }
    panic!("durable replacement rollback remained unsettled")
}

fn test_existing_destination(root: &Directory, name: &str) -> ReplaceDestination {
    let file = root
        .open_file(&LeafName::new(name).expect("destination leaf"))
        .expect("open replacement destination");
    let revision = file.revision().expect("destination revision");
    let digest = Sha256::digest(file.read_bounded(1 << 20).expect("destination bytes"));
    ReplaceDestination::Existing(
        file.park_request(ExpectedFileContent::new(revision, digest.into())),
    )
}

fn assert_test_recovery_settled(session: &RootSession, root_path: &Path) {
    let state = session
        .authority
        .operations
        .lock()
        .expect("operation state");
    assert!(!state.recovery.has_live_or_uncertain());
    assert!(state.stages.is_empty());
    drop(state);
    for entry in std::fs::read_dir(root_path).expect("list root") {
        let name = entry.expect("root entry").file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".axial-rstage-") && !name.starts_with(".axial-rpark-"),
            "recovery carrier {name} survived settlement"
        );
    }
}

#[test]
fn durable_replace_replaces_existing_and_creates_vacant_destinations() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("state.json"), b"old state").expect("old state");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    require_test_durable_replace(
        root.replace_durable(test_existing_destination(&root, "state.json"), b"new state"),
    );
    require_test_durable_replace(root.replace_durable(
        ReplaceDestination::Vacant {
            parent: root.clone(),
            name: LeafName::new("vacant.json").expect("vacant leaf"),
        },
        b"vacant state",
    ));
    assert_eq!(
        std::fs::read(temporary.path().join("state.json")).expect("replaced state"),
        b"new state"
    );
    assert_eq!(
        std::fs::read(temporary.path().join("vacant.json")).expect("created state"),
        b"vacant state"
    );
    assert_test_recovery_settled(&session, temporary.path());
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn durable_replace_rolls_back_an_uncertain_commit_before_any_rename() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("state.json"), b"old state").expect("old state");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let failure = recovery::install_pre_barrier_sync_failure_after(1);
    let outcome =
        root.replace_durable(test_existing_destination(&root, "state.json"), b"new state");
    drop(failure);
    require_test_durable_replace_no_effect(outcome);
    assert_eq!(
        std::fs::read(temporary.path().join("state.json")).expect("retained state"),
        b"old state"
    );
    assert_test_recovery_settled(&session, temporary.path());
    require_test_durable_replace(
        root.replace_durable(test_existing_destination(&root, "state.json"), b"new state"),
    );
    assert_eq!(
        std::fs::read(temporary.path().join("state.json")).expect("replaced state"),
        b"new state"
    );
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn durable_replace_retains_an_interrupted_live_replay_until_reconciled() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("state.json"), b"old state").expect("old state");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let failure = recovery::install_pre_barrier_sync_failure_after(3);
    let obligation =
        match root.replace_durable(test_existing_destination(&root, "state.json"), b"new state") {
            DurableReplaceOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("interrupted live replay was not retained: {outcome:?}"),
        };
    drop(failure);
    let competing = root.replace_durable(
        ReplaceDestination::Vacant {
            parent: root.clone(),
            name: LeafName::new("competing.json").expect("competing leaf"),
        },
        b"competing",
    );
    assert!(matches!(competing, DurableReplaceOutcome::NoEffect(_)));
    require_test_durable_replace(obligation.reconcile());
    assert_eq!(
        std::fs::read(temporary.path().join("state.json")).expect("replaced state"),
        b"new state"
    );
    assert!(!temporary.path().join("competing.json").exists());
    assert_test_recovery_settled(&session, temporary.path());
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn durable_replace_rejects_bounds_and_aliases_before_io() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("existing.json"), b"existing").expect("existing");
    std::fs::create_dir(temporary.path().join("nested")).expect("nested directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let vacant = |parent: &Directory, name: &str| ReplaceDestination::Vacant {
        parent: parent.clone(),
        name: LeafName::new(name).expect("destination leaf"),
    };
    let nested = root
        .open_directory(&LeafName::new("nested").expect("nested leaf"))
        .expect("nested capability");
    for (destination, contents) in [
        (vacant(&root, "EXISTING.json"), Vec::new()),
        (vacant(&root, "existing.json"), Vec::new()),
        (vacant(&nested, "foreign.json"), Vec::new()),
        (
            vacant(&root, "oversized.json"),
            vec![0_u8; recovery::MAX_RECOVERABLE_FILE_BYTES as usize + 1],
        ),
    ] {
        assert!(matches!(
            root.replace_durable(destination, &contents),
            DurableReplaceOutcome::NoEffect(_)
        ));
    }
    assert_test_recovery_settled(&session, temporary.path());
    assert!(!temporary.path().join("oversized.json").exists());
    drop((nested, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

/// Crashes a child process immediately after each journal write of a live replacement,
/// before its barrier, then proves the next acquisition settles it to old or new bytes.
#[test]
fn durable_replace_recovers_from_a_crash_after_every_journal_write() {
    const CHILD: &str = "AXIAL_TEST_DURABLE_REPLACE_CRASH";
    if let Some(request) = std::env::var_os(CHILD) {
        let request = request.into_string().expect("child request");
        let (writes, path) = request.split_once(':').expect("child request shape");
        let session = acquire_test_root(Path::new(path));
        let root = session.root().expect("child root capability");
        let destination = test_existing_destination(&root, "state.json");
        let _crash =
            recovery::install_pre_barrier_crash_after(writes.parse().expect("child crash point"));
        require_test_durable_replace(root.replace_durable(destination, b"new state"));
        std::process::exit(0);
    }
    let mut observed = Vec::new();
    for writes in 0..16 {
        let temporary = crate::test_tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("state.json"), b"old state").expect("old state");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "tests::durable_replace_recovers_from_a_crash_after_every_journal_write",
                "--exact",
                "--nocapture",
            ])
            .env(
                CHILD,
                format!(
                    "{writes}:{}",
                    temporary.path().to_str().expect("UTF-8 root")
                ),
            )
            .status()
            .expect("run crashing child");
        let crashed = match status.code() {
            Some(recovery::CRASH_EXIT_CODE) => true,
            Some(0) => false,
            _ => panic!("crash child failed at write {writes}: {status:?}"),
        };
        let session = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("acquisition did not recover write {writes}: {outcome:?}"),
        };
        let contents = std::fs::read(temporary.path().join("state.json")).expect("recovered state");
        assert!(
            contents == b"old state" || contents == b"new state",
            "write {writes} recovered foreign bytes"
        );
        assert_test_recovery_settled(&session, temporary.path());
        let root = session.root().expect("recovered root");
        require_test_durable_replace(
            root.replace_durable(test_existing_destination(&root, "state.json"), b"retry"),
        );
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        observed.push(contents == b"new state");
        if !crashed {
            break;
        }
    }
    assert_eq!(observed.last(), Some(&true), "replacement never completed");
    assert!(observed.len() >= 6, "too few crash points: {observed:?}");
    assert!(
        !observed[0],
        "a crash before the commit published new bytes"
    );
    let first_new = observed.iter().position(|new| *new).expect("new bytes");
    assert!(
        observed[first_new..].iter().all(|new| *new),
        "recovery regressed after the commit point: {observed:?}"
    );
}

#[test]
fn uncertain_recovery_seal_becomes_read_only_and_reseals_idempotently() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let target = LeafName::new("uncertain-seal.bin").expect("target leaf");
    let mut staged = match root.create_recoverable_stage(&target) {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => panic!("recovery create failed: {error}"),
        FileCreateOutcome::AppliedUnverified(obligation) => {
            panic!("recovery create was not verified: {}", obligation.error())
        }
    };
    staged
        .write_all(b"sealed despite uncertain sync")
        .expect("stage payload");
    let stage_path = temporary.path().join(staged.file.name.as_os_str());
    let hook = recovery::install_pre_barrier_sync_failure();
    let failure = staged
        .seal()
        .expect_err("injected recovery sync failure must retain the stage");
    drop(hook);
    assert!(
        session
            .authority
            .operations
            .lock()
            .expect("recovery state")
            .recovery
            .is_uncertain()
    );

    let mut staged = failure.into_staged();
    assert_eq!(
        staged
            .writer()
            .expect_err("durably sealed carrier must not become writable")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        std::fs::read(&stage_path).expect("read unchanged stage"),
        b"sealed despite uncertain sync"
    );
    let sealed = staged
        .seal()
        .expect("exact reconciled seal must be idempotent");
    require_test_stage_discard(sealed.discard());
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn sealed_recovery_reserves_target_until_durable_cancellation() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let target = LeafName::new("reserved-target.bin").expect("target leaf");
    let (recovery, _) = test_recoverable_stage(&root, &target, b"recovery payload");
    let ordinary = test_sealed_stage(&root, b"ordinary payload");

    let ordinary =
        require_test_promotion_no_effect(ordinary.promote_no_replace(&root, &root, &target));
    require_test_stage_discard(recovery.discard());
    let published = require_test_promotion(ordinary.promote_no_replace(&root, &root, &target));
    assert_eq!(
        published.read_bounded(32).expect("published payload"),
        b"ordinary payload"
    );
    drop((published, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn recovery_seal_refuses_an_existing_target_reservation() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let target = LeafName::new("reserved-before-seal.bin").expect("target leaf");
    let ordinary = test_sealed_stage(&root, b"ordinary payload");
    let (attempt_id, receipt) = test_publication_attempt(&ordinary, &root, &target);
    ordinary
        .token
        .prepare_promotion(&root, &target, attempt_id, receipt, None, None)
        .expect("reserve ordinary publication target");

    let mut recovery = match root.create_recoverable_stage(&target) {
        FileCreateOutcome::Created(staged) => staged,
        FileCreateOutcome::NoEffect(error) => panic!("recovery create failed: {error}"),
        FileCreateOutcome::AppliedUnverified(obligation) => {
            panic!("recovery create was not verified: {}", obligation.error())
        }
    };
    recovery
        .write_all(b"recovery payload")
        .expect("recovery payload");
    let failure = recovery
        .seal()
        .expect_err("existing target reservation must block durable seal");
    ordinary
        .token
        .update(StageRegistryPhase::Sealed)
        .expect("release ordinary publication reservation");
    let sealed = failure
        .into_staged()
        .seal()
        .expect("recovery seal after reservation release");
    require_test_stage_discard(sealed.discard());
    require_test_stage_discard(ordinary.discard());
    drop(root);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn prepared_recovery_replay_ignores_user_target_and_create_only_park() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let operation_id = [0x71; 16];
    let (registration, stage) = persist_test_recovery_fixture(
        &first,
        temporary.path(),
        operation_id,
        "user-target.bin",
        RecoveryPhase::StagePrepared,
        b"unsealed stage",
        true,
    );
    let park = recovery_park_leaf(operation_id);
    std::fs::write(temporary.path().join("user-target.bin"), b"user target").expect("user target");
    std::fs::write(temporary.path().join(park.as_str()), b"user park").expect("user park");
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    assert!(!temporary.path().join(stage.as_str()).exists());
    assert_eq!(
        std::fs::read(temporary.path().join("user-target.bin")).expect("read user target"),
        b"user target"
    );
    assert_eq!(
        std::fs::read(temporary.path().join(park.as_str())).expect("read user park"),
        b"user park"
    );
    let root = replayed.root().expect("replayed root");
    let target_file = root
        .open_file(&LeafName::new("user-target.bin").expect("target leaf"))
        .expect("prepared recovery does not reserve target");
    let park_file = root
        .open_file(&LeafName::new(park.as_str()).expect("park leaf"))
        .expect("create-only recovery does not reserve park");
    {
        let state = replayed.authority.operations.lock().expect("replay state");
        #[cfg(unix)]
        assert!(state.recovery.record(registration).is_none());
        #[cfg(windows)]
        {
            assert_eq!(
                state
                    .recovery
                    .record(registration)
                    .expect("retained prepared recovery")
                    .phase,
                RecoveryPhase::StagePrepared
            );
            let orphan = state
                .recovery_orphans
                .iter()
                .find(|orphan| orphan.registration == registration)
                .expect("prepared recovery orphan");
            assert!(orphan.files.is_empty());
        }
    }
    drop((target_file, park_file, root));
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replay_cancels_sealed_and_unpublished_prepared_stages_on_target_collision() {
    for (phase, operation_byte) in [
        (RecoveryPhase::StageSealed, 0x72),
        (RecoveryPhase::PublishPrepared, 0x73),
    ] {
        let temporary = crate::test_tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let (registration, stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [operation_byte; 16],
            "occupied.bin",
            phase,
            b"owned stage",
            true,
        );
        std::fs::write(temporary.path().join("occupied.bin"), b"unrelated target")
            .expect("unrelated target");
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert_eq!(
            std::fs::read(temporary.path().join("occupied.bin")).expect("read unrelated target"),
            b"unrelated target"
        );
        let root = replayed.root().expect("replayed root");
        let target = root
            .open_file(&LeafName::new("occupied.bin").expect("target leaf"))
            .expect("cancelled recovery does not reserve target");
        {
            let state = replayed.authority.operations.lock().expect("replay state");
            #[cfg(unix)]
            {
                assert!(state.recovery.record(registration).is_none());
                assert!(
                    state
                        .recovery_orphans
                        .iter()
                        .all(|orphan| orphan.registration != registration)
                );
            }
            #[cfg(windows)]
            {
                assert_eq!(
                    state
                        .recovery
                        .record(registration)
                        .expect("retained cancellation")
                        .phase,
                    RecoveryPhase::RemovePrepared
                );
                let orphan = state
                    .recovery_orphans
                    .iter()
                    .find(|orphan| orphan.registration == registration)
                    .expect("cancellation orphan");
                assert!(orphan.files.is_empty());
            }
        }
        drop((target, root));
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }
}

#[cfg(unix)]
#[test]
fn sealed_recovery_without_its_carrier_fails_closed_and_preserves_target() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let (_, stage) = persist_test_recovery_fixture(
        &first,
        temporary.path(),
        [0x74; 16],
        "unrelated.bin",
        RecoveryPhase::StageSealed,
        b"same bytes",
        false,
    );
    std::fs::write(temporary.path().join("unrelated.bin"), b"same bytes")
        .expect("unrelated target");
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let obligation = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("missing sealed carrier did not fail closed: {outcome:?}"),
    };
    assert!(!temporary.path().join(stage.as_str()).exists());
    assert_eq!(
        std::fs::read(temporary.path().join("unrelated.bin")).expect("preserved unrelated target"),
        b"same bytes"
    );
    obligation
        .acknowledge_preserved()
        .expect("preserve failed recovery root");
}

#[test]
fn recoverable_discard_releases_target_in_the_same_session() {
    for publish_prepared in [false, true] {
        let temporary = crate::test_tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("after-cancel.bin").expect("target leaf");
        if !publish_prepared {
            std::fs::write(
                temporary.path().join("after-cancel.bin"),
                b"preexisting user target",
            )
            .expect("preexisting user target");
        }
        let (staged, registration) = test_recoverable_stage(&root, &target, b"owned payload");
        let staged = if publish_prepared {
            std::fs::write(
                temporary.path().join("after-cancel.bin"),
                b"unrelated target",
            )
            .expect("unrelated target");
            let staged =
                require_test_promotion_no_effect(staged.promote_no_replace(&root, &root, &target));
            assert_eq!(
                session
                    .authority
                    .operations
                    .lock()
                    .expect("recovery state")
                    .recovery
                    .record(registration)
                    .expect("publish-prepared record")
                    .phase,
                RecoveryPhase::PublishPrepared
            );
            staged
        } else {
            staged
        };
        require_test_stage_discard(staged.discard());
        if !publish_prepared {
            std::fs::write(
                temporary.path().join("after-cancel.bin"),
                b"new user target",
            )
            .expect("new user target");
        }
        let opened = root
            .open_file(&target)
            .expect("cancelled recovery releases target coordinate");
        {
            let state = session.authority.operations.lock().expect("recovery state");
            assert!(state.recovery.record(registration).is_none());
            assert!(
                state
                    .recovery_orphans
                    .iter()
                    .all(|orphan| orphan.registration != registration)
            );
        }
        drop((opened, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }
}

#[test]
fn publish_prepared_no_effect_can_retry_exactly() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let target = LeafName::new("retried.bin").expect("target leaf");
    let (staged, registration) = test_recoverable_stage(&root, &target, b"retry payload");
    std::fs::write(temporary.path().join("retried.bin"), b"collision")
        .expect("promotion collision");
    let staged = require_test_promotion_no_effect(staged.promote_no_replace(&root, &root, &target));
    assert_eq!(
        session
            .authority
            .operations
            .lock()
            .expect("recovery state")
            .recovery
            .record(registration)
            .expect("publish-prepared record")
            .phase,
        RecoveryPhase::PublishPrepared
    );
    std::fs::remove_file(temporary.path().join("retried.bin")).expect("remove promotion collision");

    let published = require_test_promotion(staged.promote_no_replace(&root, &root, &target));
    assert_eq!(
        std::fs::read(temporary.path().join("retried.bin")).expect("retried target"),
        b"retry payload"
    );
    {
        let state = session.authority.operations.lock().expect("recovery state");
        #[cfg(unix)]
        assert!(state.recovery.record(registration).is_none());
        #[cfg(windows)]
        {
            assert_eq!(
                state
                    .recovery
                    .record(registration)
                    .expect("committed publication")
                    .phase,
                RecoveryPhase::RemoveCommitted
            );
            let orphan = state
                .recovery_orphans
                .iter()
                .find(|orphan| orphan.registration == registration)
                .expect("publication orphan");
            assert!(orphan.files.as_slice() == [published.identity]);
        }
    }
    drop((published, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn reconciled_applied_publication_completes_recovery_before_disarm() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let target = LeafName::new("reconciled.bin").expect("target leaf");
    let (staged, registration) = test_recoverable_stage(&root, &target, b"reconciled payload");
    let authority = root.authority().expect("recovery authority");
    let operation = authority.enter().expect("publication operation");
    let expected = validate_recovery_publication(
        &authority,
        &operation,
        &staged.token,
        &staged.file,
        &staged.revision,
        &root,
        &root,
        &target,
        None,
    )
    .expect("validate recoverable publication");
    let (attempt_id, mut receipt) = test_publication_attempt(&staged, &root, &target);
    staged
        .token
        .prepare_promotion(
            &root,
            &target,
            attempt_id,
            receipt.clone(),
            None,
            expected.as_ref(),
        )
        .expect("prepare recoverable publication");
    platform::rename_no_replace(
        &mut receipt,
        attempt_id,
        root.inner.handle(),
        staged.file.name.as_os_str(),
        &staged.file.handle,
        root.inner.handle(),
        target.as_os_str(),
    )
    .expect("apply publication before reconciliation");
    staged.token.record_publication(attempt_id, receipt.clone());
    drop(operation);
    let resolution = FilePromotionObligation {
        error: io::Error::other("test publication requires reconciliation"),
        retained: staged,
        destination: root.clone(),
        destination_name: target.clone(),
        attempt_id,
        receipt,
    }
    .reconcile();
    let published = match resolution {
        FilePromotionResolution::Applied(file) => file,
        FilePromotionResolution::NoEffect(staged) => {
            require_test_stage_discard(staged.discard());
            panic!("applied publication reconciled to no effect")
        }
        FilePromotionResolution::Indeterminate(obligation) => panic!(
            "applied publication remained indeterminate: {}",
            obligation.error()
        ),
    };
    assert_eq!(
        std::fs::read(temporary.path().join("reconciled.bin")).expect("reconciled publication"),
        b"reconciled payload"
    );
    {
        let state = session.authority.operations.lock().expect("recovery state");
        assert!(state.stages.is_empty());
        #[cfg(unix)]
        assert!(state.recovery.record(registration).is_none());
        #[cfg(windows)]
        {
            assert_eq!(
                state
                    .recovery
                    .record(registration)
                    .expect("committed publication")
                    .phase,
                RecoveryPhase::RemoveCommitted
            );
            let orphan = state
                .recovery_orphans
                .iter()
                .find(|orphan| orphan.registration == registration)
                .expect("publication orphan");
            assert!(orphan.files.as_slice() == [published.identity]);
        }
    }
    drop((published, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn uncertain_startup_replay_retains_exclusive_stage_until_retry() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let operation_id = [0x5a; 16];
    let payload = b"restart payload";
    let (_, stage_name) = persist_test_recovery_fixture(
        &first,
        temporary.path(),
        operation_id,
        "replayed.bin",
        RecoveryPhase::StageSealed,
        payload,
        true,
    );
    let stage_path = temporary.path().join(stage_name.as_str());
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    recovery::replay::fail_next_replay_exclusive_admission();
    let obligation = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("uncertain startup replay did not retain ownership: {outcome:?}"),
    };
    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
    ));
    let obligation = obligation
        .cleanup()
        .expect_err("acquired replay ownership must refuse cleanup");
    let phase_failure = recovery::install_pre_barrier_sync_failure();
    let obligation = match obligation.reconcile() {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("publication phase did not retain replay: {outcome:?}"),
    };
    drop(phase_failure);
    let settlement_failure = recovery::install_pre_barrier_sync_failure();
    let obligation = match obligation.reconcile() {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("publication settlement did not retain replay: {outcome:?}"),
    };
    drop(settlement_failure);
    let replayed = match obligation.reconcile() {
        RootSessionAcquireOutcome::Acquired(session) => session,
        outcome => panic!("settled startup replay did not reconcile: {outcome:?}"),
    };
    assert_eq!(
        std::fs::read(temporary.path().join("replayed.bin")).expect("replayed target"),
        payload
    );
    assert!(!stage_path.exists());
    #[cfg(unix)]
    assert_eq!(
        recovery::replay::take_replay_tombstone_prunes(),
        1,
        "confirmed tombstone must retire the exclusive carrier after receipt settlement",
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replay_transfer_preflight_failure_moves_neither_of_two_carriers() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    for (operation, destination, payload) in [
        (
            [0x5b; 16],
            "first-replayed.bin",
            b"first payload".as_slice(),
        ),
        (
            [0x5c; 16],
            "second-replayed.bin",
            b"second payload".as_slice(),
        ),
    ] {
        persist_test_recovery_fixture(
            &first,
            temporary.path(),
            operation,
            destination,
            RecoveryPhase::StageSealed,
            payload,
            true,
        );
    }
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let phase_failure = recovery::install_pre_barrier_sync_failure();
    let obligation = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("initial replay did not retain both carriers: {outcome:?}"),
    };
    drop(phase_failure);
    assert_eq!(recovery::replay::take_replay_transfer_commits(), 0);

    recovery::replay::fail_replay_transfer_after(2);
    let obligation = match obligation.reconcile() {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("second transfer mapping did not fail in preflight: {outcome:?}"),
    };
    assert_eq!(
        recovery::replay::take_replay_transfer_commits(),
        0,
        "mapping two failed after validation, so mapping one must not move",
    );

    let replayed = match obligation.reconcile() {
        RootSessionAcquireOutcome::Acquired(session) => session,
        outcome => panic!("unchanged retained carriers did not replay: {outcome:?}"),
    };
    assert_eq!(recovery::replay::take_replay_transfer_commits(), 2);
    assert_eq!(
        std::fs::read(temporary.path().join("first-replayed.bin")).expect("first replayed target"),
        b"first payload",
    );
    assert_eq!(
        std::fs::read(temporary.path().join("second-replayed.bin"))
            .expect("second replayed target"),
        b"second payload",
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replacement_replay_completes_the_full_commit_sequence() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"old replacement payload".as_slice();
    let new_payload = b"new replacement payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x81; 16],
        "replacement.bin",
        RecoveryPhase::StageSealed,
        old_payload,
        new_payload,
        [Some(new_payload), Some(old_payload), None],
    );
    let stage_path = temporary.path().join(stage.as_str());
    let park_path = temporary.path().join(park.as_str());
    let stage_identity = test_file_identity(&stage_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    let target_path = temporary.path().join("replacement.bin");
    assert_eq!(
        std::fs::read(&target_path).expect("replacement target"),
        new_payload
    );
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(!stage_path.exists());
    assert!(!park_path.exists());
    assert_test_recovery_terminal(
        &replayed,
        registration,
        RecoveryPhase::RemoveCommitted,
        &[stage_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replacement_replay_cancels_without_touching_a_foreign_target() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"expected old payload".as_slice();
    let new_payload = b"cancelled replacement payload".as_slice();
    let foreign_payload = b"unrelated target payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x82; 16],
        "foreign.bin",
        RecoveryPhase::StageSealed,
        old_payload,
        new_payload,
        [Some(new_payload), Some(foreign_payload), None],
    );
    let target_path = temporary.path().join("foreign.bin");
    let foreign_identity = test_file_identity(&target_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    assert_eq!(
        std::fs::read(&target_path).expect("foreign target"),
        foreign_payload
    );
    assert!(test_file_identity(&target_path) == foreign_identity);
    assert!(!temporary.path().join(stage.as_str()).exists());
    assert!(!temporary.path().join(park.as_str()).exists());
    assert_test_recovery_terminal(&replayed, registration, RecoveryPhase::RemovePrepared, &[]);
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replacement_prepared_stage_cleanup_ignores_the_user_target() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"expected old payload".as_slice();
    let staged_payload = b"unsealed replacement payload".as_slice();
    let user_payload = b"user target payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x89; 16],
        "prepared-user-target.bin",
        RecoveryPhase::StagePrepared,
        old_payload,
        staged_payload,
        [Some(staged_payload), Some(user_payload), None],
    );
    let target_path = temporary.path().join("prepared-user-target.bin");
    let user_identity = test_file_identity(&target_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    assert_eq!(
        std::fs::read(&target_path).expect("preserved user target"),
        user_payload
    );
    assert!(test_file_identity(&target_path) == user_identity);
    assert!(!temporary.path().join(stage.as_str()).exists());
    assert!(!temporary.path().join(park.as_str()).exists());
    assert_test_recovery_terminal(&replayed, registration, RecoveryPhase::StagePrepared, &[]);
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replacement_replay_restores_an_interrupted_park() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"restored old payload".as_slice();
    let new_payload = b"unpublished replacement payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x83; 16],
        "restored.bin",
        RecoveryPhase::ReplacePrepared,
        old_payload,
        new_payload,
        [None, None, Some(old_payload)],
    );
    let park_path = temporary.path().join(park.as_str());
    let park_identity = test_file_identity(&park_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    let target_path = temporary.path().join("restored.bin");
    assert_eq!(
        std::fs::read(&target_path).expect("restored target"),
        old_payload
    );
    assert!(test_file_identity(&target_path) == park_identity);
    assert!(!temporary.path().join(stage.as_str()).exists());
    assert!(!park_path.exists());
    assert_test_recovery_terminal(&replayed, registration, RecoveryPhase::RemovePrepared, &[]);
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn equal_proof_replacement_prefers_the_stage_carrier() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let payload = b"equal proof payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x84; 16],
        "equal.bin",
        RecoveryPhase::StageSealed,
        payload,
        payload,
        [Some(payload), Some(payload), None],
    );
    let stage_path = temporary.path().join(stage.as_str());
    let target_path = temporary.path().join("equal.bin");
    let stage_identity = test_file_identity(&stage_path);
    let old_target_identity = test_file_identity(&target_path);
    assert!(stage_identity != old_target_identity);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    assert_eq!(std::fs::read(&target_path).expect("equal target"), payload);
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(test_file_identity(&target_path) != old_target_identity);
    assert!(!stage_path.exists());
    assert!(!temporary.path().join(park.as_str()).exists());
    assert_test_recovery_terminal(
        &replayed,
        registration,
        RecoveryPhase::RemoveCommitted,
        &[stage_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn replacement_replay_retries_settlement_after_park_removal() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"removed park payload".as_slice();
    let new_payload = b"already published payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x85; 16],
        "settlement-retry.bin",
        RecoveryPhase::PublishPrepared,
        old_payload,
        new_payload,
        [None, Some(new_payload), Some(old_payload)],
    );
    let target_path = temporary.path().join("settlement-retry.bin");
    let target_identity = test_file_identity(&target_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    recovery::replay::fail_next_replay_removal_settlement();
    let obligation = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("post-removal settlement did not retain replay: {outcome:?}"),
    };
    assert_eq!(
        std::fs::read(&target_path).expect("retained published target"),
        new_payload
    );
    assert!(test_file_identity(&target_path) == target_identity);
    assert!(!temporary.path().join(stage.as_str()).exists());
    assert!(!temporary.path().join(park.as_str()).exists());
    assert!(matches!(
        RootSession::acquire(temporary.path()),
        RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
    ));
    assert_eq!(
        recovery::replay::take_replay_pending_removal_settlements(),
        0,
        "the injected cut must occur before pending settlement starts",
    );
    let obligation = obligation
        .cleanup()
        .expect_err("armed replacement replay must refuse cleanup");

    let replayed = match obligation.reconcile() {
        RootSessionAcquireOutcome::Acquired(session) => session,
        outcome => panic!("post-removal settlement did not reconcile: {outcome:?}"),
    };
    assert_eq!(
        recovery::replay::take_replay_pending_removal_settlements(),
        1,
        "retry must settle the typed removed carrier before classification",
    );
    assert_test_recovery_terminal(
        &replayed,
        registration,
        RecoveryPhase::RemoveCommitted,
        &[target_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn replacement_publication_retries_a_poisoned_recovery_receipt() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"parked publication payload".as_slice();
    let new_payload = b"published after retry payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x8a; 16],
        "poisoned-retry.bin",
        RecoveryPhase::PublishPrepared,
        old_payload,
        new_payload,
        [Some(new_payload), None, Some(old_payload)],
    );
    let stage_path = temporary.path().join(stage.as_str());
    let target_path = temporary.path().join("poisoned-retry.bin");
    let park_path = temporary.path().join(park.as_str());
    let stage_identity = test_file_identity(&stage_path);
    let park_identity = test_file_identity(&park_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let first_failure =
        platform::install_publication_directory_sync_test_outcomes([Err(io::ErrorKind::Other)]);
    let obligation = match RootSession::acquire(temporary.path()) {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("publication barrier did not retain replay: {outcome:?}"),
    };
    drop(first_failure);
    assert!(!stage_path.exists());
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(test_file_identity(&park_path) == park_identity);

    let second_failure =
        platform::install_publication_directory_sync_test_outcomes([Err(io::ErrorKind::Other)]);
    let obligation = match obligation.reconcile() {
        RootSessionAcquireOutcome::AppliedUnverified(obligation)
            if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
        {
            obligation
        }
        outcome => panic!("poisoned publication receipt was not retryable: {outcome:?}"),
    };
    drop(second_failure);
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(test_file_identity(&park_path) == park_identity);

    let successful_barriers =
        platform::install_publication_directory_sync_test_outcomes([Ok(()), Ok(())]);
    let replayed = match obligation.reconcile() {
        RootSessionAcquireOutcome::Acquired(session) => session,
        outcome => panic!("poisoned publication receipt did not settle: {outcome:?}"),
    };
    drop(successful_barriers);
    assert_eq!(
        std::fs::read(&target_path).expect("settled replacement target"),
        new_payload
    );
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(!park_path.exists());
    assert_test_recovery_terminal(
        &replayed,
        registration,
        RecoveryPhase::RemoveCommitted,
        &[stage_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn remove_committed_replay_finishes_the_durable_desired_state() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let old_payload = b"durably superseded payload".as_slice();
    let new_payload = b"durable desired payload".as_slice();
    let (registration, stage, park) = persist_test_replacement_fixture(
        &first,
        temporary.path(),
        [0x86; 16],
        "durable-desired.bin",
        RecoveryPhase::RemoveCommitted,
        old_payload,
        new_payload,
        [Some(new_payload), Some(old_payload), None],
    );
    let stage_path = temporary.path().join(stage.as_str());
    let stage_identity = test_file_identity(&stage_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    let target_path = temporary.path().join("durable-desired.bin");
    assert_eq!(
        std::fs::read(&target_path).expect("durable desired target"),
        new_payload
    );
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(!stage_path.exists());
    assert!(!temporary.path().join(park.as_str()).exists());
    assert_test_recovery_terminal(
        &replayed,
        registration,
        RecoveryPhase::RemoveCommitted,
        &[stage_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn create_only_remove_committed_republishes_its_stage() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let payload = b"create-only durable payload".as_slice();
    let (registration, stage) = persist_test_recovery_fixture(
        &first,
        temporary.path(),
        [0x8b; 16],
        "create-only-durable.bin",
        RecoveryPhase::RemoveCommitted,
        payload,
        true,
    );
    let stage_path = temporary.path().join(stage.as_str());
    let stage_identity = test_file_identity(&stage_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    let target_path = temporary.path().join("create-only-durable.bin");
    assert_eq!(
        std::fs::read(&target_path).expect("create-only durable target"),
        payload
    );
    assert!(test_file_identity(&target_path) == stage_identity);
    assert!(!stage_path.exists());
    assert_test_recovery_terminal(
        &replayed,
        registration,
        RecoveryPhase::RemoveCommitted,
        &[stage_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn create_only_and_replacement_replay_share_one_parent_snapshot() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let first = acquire_test_root(temporary.path());
    let create_payload = b"create-only payload".as_slice();
    let (create_registration, create_stage) = persist_test_recovery_fixture(
        &first,
        temporary.path(),
        [0x87; 16],
        "created.bin",
        RecoveryPhase::StageSealed,
        create_payload,
        true,
    );
    let old_payload = b"mixed old payload".as_slice();
    let new_payload = b"mixed replacement payload".as_slice();
    let (replacement_registration, replacement_stage, replacement_park) =
        persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x88; 16],
            "mixed-replacement.bin",
            RecoveryPhase::StageSealed,
            old_payload,
            new_payload,
            [Some(new_payload), Some(old_payload), None],
        );
    let create_stage_path = temporary.path().join(create_stage.as_str());
    let replacement_stage_path = temporary.path().join(replacement_stage.as_str());
    let create_identity = test_file_identity(&create_stage_path);
    let replacement_identity = test_file_identity(&replacement_stage_path);
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let replayed = acquire_test_root(temporary.path());
    let created_path = temporary.path().join("created.bin");
    let replacement_path = temporary.path().join("mixed-replacement.bin");
    assert_eq!(
        std::fs::read(&created_path).expect("created target"),
        create_payload
    );
    assert_eq!(
        std::fs::read(&replacement_path).expect("replacement target"),
        new_payload
    );
    assert!(test_file_identity(&created_path) == create_identity);
    assert!(test_file_identity(&replacement_path) == replacement_identity);
    assert!(!create_stage_path.exists());
    assert!(!replacement_stage_path.exists());
    assert!(!temporary.path().join(replacement_park.as_str()).exists());
    assert_test_recovery_terminal(
        &replayed,
        create_registration,
        RecoveryPhase::RemoveCommitted,
        &[create_identity],
    );
    assert_test_recovery_terminal(
        &replayed,
        replacement_registration,
        RecoveryPhase::RemoveCommitted,
        &[replacement_identity],
    );
    assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn unresolved_recovery_replay_drop_aborts() {
    const CHILD: &str = "AXIAL_TEST_DROP_ARMED_RECOVERY_REPLAY";
    if std::env::var_os(CHILD).is_some() {
        let temporary = crate::test_tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [0x5d; 16],
            "unresolved.bin",
            RecoveryPhase::StageSealed,
            b"unresolved payload",
            true,
        );
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));
        recovery::replay::fail_next_replay_exclusive_admission();
        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("child did not acquire armed replay ownership: {outcome:?}"),
        };
        drop(obligation);
        panic!("dropping armed replay ownership returned");
    }

    use std::os::unix::process::ExitStatusExt as _;
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .arg("unresolved_recovery_replay_drop_aborts")
        .arg("--nocapture")
        .env(CHILD, "1")
        .status()
        .expect("run armed replay drop child");
    assert_eq!(status.signal(), Some(6));
}

#[cfg(any(unix, windows))]
#[test]
fn replay_parent_swap_after_planning_retains_the_record_until_binding_restoration() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let destination = temporary.path().join("destination").join("nested");
    let displaced = temporary.path().join("displaced-nested-destination");
    std::fs::create_dir(temporary.path().join("destination")).expect("outer recovery destination");
    std::fs::create_dir(&destination).expect("nested recovery destination");
    let first = acquire_test_root(temporary.path());
    let operation_id = [0x6b; 16];
    let stage_name = recovery::recovery_stage_leaf(operation_id);
    let target = recovery::RecoveryName::new_exact("replayed.bin").expect("target name");
    let payload = b"parent-bound restart payload";
    let mut journal = recovery::RecoveryJournal::load(&first.authority.lease)
        .expect("load independent recovery fixture journal");
    let mut record = recovery::RecoveryRecord {
        operation_id,
        phase: recovery::RecoveryPhase::StagePrepared,
        destination_parent: vec![
            recovery::RecoveryName::new_exact("destination").expect("outer parent name"),
            recovery::RecoveryName::new_exact("nested").expect("nested parent name"),
        ],
        destination_leaf: target,
        old: None,
        new: None,
    };
    let registration = journal.reserve(&record).expect("reserve recovery slot");
    journal
        .create_reserved(&first.authority.lease, registration, record.clone())
        .expect("persist prepared stage");
    let stage_path = destination.join(stage_name.as_str());
    std::fs::write(&stage_path, payload).expect("write recovery fixture stage");
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&stage_path)
        .expect("open recovery fixture stage")
        .sync_all()
        .expect("sync recovery fixture stage");
    record.phase = recovery::RecoveryPhase::StageSealed;
    record.new = Some(recovery::RecoveryFileProof {
        size: payload.len() as u64,
        sha256: Sha256::digest(payload).into(),
    });
    journal
        .advance(&first.authority.lease, registration, record)
        .expect("persist sealed stage");
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    let destination_for_hook = destination.clone();
    let displaced_for_hook = displaced.clone();
    #[cfg(unix)]
    recovery::replay::set_replay_parent_validation_hook(move || {
        std::fs::rename(&destination_for_hook, &displaced_for_hook)
            .expect("displace replay parent");
        std::fs::create_dir(&destination_for_hook).expect("replace replay parent");
    });
    #[cfg(windows)]
    recovery::replay::set_replay_parent_validation_hook(move || {
        std::fs::rename(&destination_for_hook, &displaced_for_hook)
            .expect_err("retained replay parent must prevent displacement");
        assert!(destination_for_hook.is_dir());
        assert!(!displaced_for_hook.exists());
    });

    #[cfg(unix)]
    {
        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("replay parent swap did not fail closed: {outcome:?}"),
        };
        assert!(!destination.join("replayed.bin").exists());
        assert!(displaced.join(stage_name.as_str()).is_file());

        std::fs::remove_dir(&destination).expect("remove replacement parent");
        std::fs::rename(&displaced, &destination).expect("restore exact replay parent");
        let replayed = match obligation.reconcile() {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("restored replay parent did not reconcile: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(destination.join("replayed.bin")).expect("replayed payload"),
            payload
        );
        assert!(!stage_path.exists());
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    {
        let replayed = acquire_test_root(temporary.path());
        assert_eq!(
            std::fs::read(destination.join("replayed.bin")).expect("replayed payload"),
            payload
        );
        assert!(!stage_path.exists());
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }
}

#[cfg(unix)]
#[test]
fn root_lease_rejects_links_and_non_files_without_mutating_them() {
    use std::os::unix::fs::symlink;

    for fixture in ["symlink", "hardlink", "directory"] {
        let temporary = crate::test_tempdir().expect("temporary root");
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        match fixture {
            "symlink" => {
                std::fs::write(temporary.path().join("target"), b"target").expect("symlink target");
                symlink("target", &lease).expect("lease symlink");
            }
            "hardlink" => {
                let target = temporary.path().join("target");
                std::fs::write(&target, b"target").expect("hardlink target");
                std::fs::hard_link(target, &lease).expect("lease hardlink");
            }
            "directory" => std::fs::create_dir(&lease).expect("lease directory"),
            _ => unreachable!("closed fixture set"),
        }
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Lease(_))
        ));
        assert!(std::fs::symlink_metadata(&lease).is_ok());
    }
}

#[cfg(unix)]
#[test]
fn root_clear_refuses_a_replaced_lease_binding_before_deleting_children() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let owned = temporary.path().join("owned.bin");
    let lease = temporary.path().join(ROOT_LEASE_NAME);
    let displaced = temporary.path().join("displaced-lease");
    std::fs::write(&owned, b"owned").expect("owned file");
    let session = acquire_test_root(temporary.path());
    let reset = match session.begin_reset() {
        ResetStartOutcome::Ready(authority) => authority,
        outcome => panic!("reset did not become ready: {outcome:?}"),
    };
    std::fs::rename(&lease, &displaced).expect("displace retained lease");
    std::fs::write(&lease, b"replacement").expect("replacement lease");

    reset
        .try_clear_root(&[])
        .expect_err("clear accepted a replaced lease binding");
    assert!(owned.is_file(), "clear mutated children before lease proof");
    std::fs::remove_file(&lease).expect("remove replacement lease");
    std::fs::rename(&displaced, &lease).expect("restore retained lease");
    reset
        .try_clear_root(&[])
        .expect("clear recovers after lease restoration");
    assert!(!owned.exists());
    reset.release().expect("release reset authority");
}

#[cfg(windows)]
#[test]
fn windows_exclusive_lease_prevents_substitution_before_root_clear() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let owned = temporary.path().join("owned.bin");
    let lease = temporary.path().join(ROOT_LEASE_NAME);
    let displaced = temporary.path().join("displaced-lease");
    std::fs::write(&owned, b"owned").expect("owned file");
    let session = acquire_test_root(temporary.path());

    std::fs::rename(&lease, &displaced)
        .expect_err("exclusive lease must prevent pre-clear displacement");
    File::create(&lease).expect_err("exclusive lease must prevent pre-clear substitution");
    assert!(
        owned.is_file(),
        "denied substitution must not mutate children"
    );

    let reset = match session.begin_reset() {
        ResetStartOutcome::Ready(authority) => authority,
        outcome => panic!("reset did not become ready: {outcome:?}"),
    };
    reset.try_clear_root(&[]).expect("ordinary root clear");
    assert!(!owned.exists());
    reset.release().expect("release reset authority");
}

#[test]
fn directory_overlaps_compares_independent_roots_and_retained_children() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let source = temporary.path().join("source");
    let nested = source.join("nested");
    let sibling = temporary.path().join("source-sibling");
    std::fs::create_dir_all(nested.join("child")).expect("nested directories");
    std::fs::create_dir(&sibling).expect("sibling directory");
    let first = acquire_test_root(&source);
    let second = acquire_test_root(&nested);
    let root = first.root().expect("source root");
    let nested = second.root().expect("independent nested root");
    let same = second
        .admit_absolute_directory(&source)
        .expect("independently admitted source");
    let sibling = first
        .admit_absolute_directory(&sibling)
        .expect("sibling admission");
    let child = nested
        .open_directory(&LeafName::new("child").expect("child name"))
        .expect("retained child");

    for (left, right) in [
        (&root, &root),
        (&root, &same),
        (&root, &nested),
        (&root, &child),
    ] {
        assert!(left.overlaps(right).expect("overlapping directories"));
        assert!(right.overlaps(left).expect("symmetric overlap"));
    }
    for directory in [&root, &same, &nested, &child] {
        assert!(!directory.overlaps(&sibling).expect("disjoint sibling"));
        assert!(!sibling.overlaps(directory).expect("symmetric disjointness"));
    }

    drop((root, nested, same, sibling, child));
    assert!(matches!(second.revoke(), RootRevokeOutcome::Revoked));
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_overlaps_includes_the_filesystem_anchor() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("session root");
    let absolute = session
        .admit_absolute_directory(temporary.path())
        .expect("absolute root admission");
    let anchor_path = temporary
        .path()
        .ancestors()
        .last()
        .expect("filesystem anchor");
    let anchor = session
        .admit_absolute_directory(anchor_path)
        .expect("filesystem anchor admission");

    for directory in [&root, &absolute] {
        assert!(directory.overlaps(&anchor).expect("anchor contains root"));
        assert!(
            anchor
                .overlaps(directory)
                .expect("symmetric anchor overlap")
        );
    }

    drop((root, absolute, anchor));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn directory_overlaps_rejects_replaced_admitted_ancestry() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app = temporary.path().join("app");
    let source = temporary.path().join("source");
    let displaced = temporary.path().join("displaced-source");
    std::fs::create_dir(&app).expect("application root");
    std::fs::create_dir_all(source.join("child")).expect("source directories");
    let session = acquire_test_root(&app);
    let root = session.root().expect("application capability");
    let child = session
        .admit_absolute_directory(&source.join("child"))
        .expect("source child admission");
    std::fs::rename(&source, &displaced).expect("displace source ancestor");
    std::fs::create_dir_all(source.join("child")).expect("replacement source directories");

    assert!(child.overlaps(&root).is_err());
    assert!(root.overlaps(&child).is_err());
    assert!(child.overlaps(&child).is_err());

    drop((root, child));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_overlaps_rejects_either_revoked_authority() {
    let first_path = crate::test_tempdir().expect("first root");
    let second_path = crate::test_tempdir().expect("second root");
    let first = acquire_test_root(first_path.path());
    let second = acquire_test_root(second_path.path());
    let first_root = first.root().expect("first capability");
    let second_root = second.root().expect("second capability");
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

    assert!(first_root.overlaps(&second_root).is_err());
    assert!(second_root.overlaps(&first_root).is_err());
    assert!(first_root.overlaps(&first_root).is_err());

    drop((first_root, second_root));
    assert!(matches!(second.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn directory_overlaps_rejects_reparented_session_root() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let parent = temporary.path().join("parent");
    let displaced = temporary.path().join("displaced-parent");
    let other_path = temporary.path().join("other");
    std::fs::create_dir_all(parent.join("source")).expect("source root");
    std::fs::create_dir(&other_path).expect("other root");
    let first = acquire_test_root(&parent.join("source"));
    let second = acquire_test_root(&other_path);
    let source = first.root().expect("source capability");
    let other = second.root().expect("other capability");
    std::fs::rename(&parent, &displaced).expect("reparent source root");

    assert!(source.overlaps(&other).is_err());
    assert!(other.overlaps(&source).is_err());
    assert!(source.overlaps(&source).is_err());

    std::fs::rename(&displaced, &parent).expect("restore source ancestry");
    assert!(!source.overlaps(&other).expect("restored disjoint roots"));
    drop((source, other));
    assert!(matches!(second.revoke(), RootRevokeOutcome::Revoked));
    assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn directory_overlaps_refuses_disjointness_without_complete_ancestry() {
    let temporary = crate::test_tempdir().expect("temporary parent");
    let app = temporary.path().join("app");
    let library = temporary.path().join("library");
    std::fs::create_dir(&app).expect("application root");
    std::fs::create_dir_all(library.join("child")).expect("library directories");
    let session = acquire_test_root(&app);
    let admitted = session
        .admit_absolute_directory_authority(&library)
        .expect("library admission");
    let detached = match admitted.acquire_root_session().expect("acquire library") {
        RootSessionAcquireOutcome::Acquired(session) => session,
        RootSessionAcquireOutcome::NoEffect(error) => {
            panic!("library acquisition: {error}")
        }
        RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            let error = obligation.error().to_string();
            assert!(obligation.cleanup().is_ok());
            panic!("unverified library acquisition: {error}");
        }
    };
    let root = detached.root().expect("detached root");
    let child_name = LeafName::new("child").expect("child name");
    let child = root.open_directory(&child_name).expect("retained child");
    let other = session.root().expect("unrelated application root");

    assert!(root.overlaps(&root).expect("known detached equality"));
    assert!(root.overlaps(&child).expect("known retained child"));
    for directory in [&root, &child] {
        assert_eq!(
            directory
                .overlaps(&other)
                .expect_err("unknown ancestry must fail closed")
                .kind(),
            io::ErrorKind::Unsupported,
        );
        assert!(other.overlaps(directory).is_err());
    }

    drop((root, child, other));
    assert!(matches!(detached.revoke(), RootRevokeOutcome::Revoked));
    drop(admitted);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn absolute_directory_containment_uses_retained_physical_ancestry() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let nested = temporary.path().join("user-library");
    std::fs::create_dir(&nested).expect("nested directory");
    let external = crate::test_tempdir().expect("external directory");
    let session = acquire_test_root(temporary.path());

    assert!(matches!(
        session.admit_absolute_directory_authority_outside_root(temporary.path()),
        AbsoluteDirectoryOutsideRootAdmission::InsideRoot
    ));
    assert!(matches!(
        session.admit_absolute_directory_authority_outside_root(&nested),
        AbsoluteDirectoryOutsideRootAdmission::InsideRoot
    ));
    assert!(matches!(
        session.admit_absolute_directory_authority_outside_root(external.path()),
        AbsoluteDirectoryOutsideRootAdmission::Admitted(_)
    ));

    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn absolute_directory_containment_rejects_symlink_ancestry() {
    use std::os::unix::fs::symlink;

    let temporary = crate::test_tempdir().expect("temporary root");
    let external = crate::test_tempdir().expect("external directory");
    let alias = temporary.path().join("external-alias");
    symlink(external.path(), &alias).expect("external alias");
    let session = acquire_test_root(temporary.path());

    assert!(matches!(
        session.admit_absolute_directory_authority_outside_root(&alias),
        AbsoluteDirectoryOutsideRootAdmission::Unavailable(_)
    ));

    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn revoked_capabilities_refuse_operations() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        root.entries(1)
            .expect_err("revoked capability must refuse")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn leaf_name_equivalence_covers_case_folding_and_normalization() {
    let bounded_portable_key = |name: &OsStr| {
        let mut portable = Vec::with_capacity(platform::MAX_EQUIVALENCE_KEY_BYTES);
        let mut native = Vec::with_capacity(platform::MAX_EQUIVALENCE_KEY_BYTES);
        let mut normalization = Vec::with_capacity(platform::MAX_EQUIVALENCE_KEY_BYTES);
        assert!(
            platform::fill_leaf_name_equivalence_keys(
                name,
                &mut portable,
                &mut native,
                &mut normalization,
            )
            .expect("bounded equivalence keys")
        );
        LeafNameEquivalenceKey(portable)
    };
    let equivalent = |first: &OsStr, second: &OsStr| {
        assert!(platform::leaf_names_equal(first, second));
        let first_keys = leaf_name_equivalence_keys(first);
        let second_keys = leaf_name_equivalence_keys(second);
        assert!(first_keys.iter().any(|key| second_keys.contains(key)));
        assert_eq!(first_keys[0], bounded_portable_key(first));
        assert_eq!(bounded_portable_key(first), bounded_portable_key(second));
    };
    equivalent(OsStr::new("state.json"), OsStr::new("STATE.JSON"));
    equivalent(OsStr::new("Stra\u{00df}e"), OsStr::new("STRASSE"));
    equivalent(OsStr::new("\u{00e9}"), OsStr::new("E\u{0301}"));
    equivalent(
        OsStr::new("a\u{0323}\u{0301}"),
        OsStr::new("A\u{0301}\u{0323}"),
    );
    equivalent(OsStr::new("\u{1e09}"), OsStr::new("C\u{0327}\u{0301}"));
    assert!(!platform::leaf_names_equal(
        OsStr::new("state.json"),
        OsStr::new("other.json"),
    ));
}

#[cfg(unix)]
#[test]
fn non_utf8_leaf_keys_preserve_exact_native_identity() {
    use std::os::unix::ffi::OsStrExt;

    let first = OsStr::from_bytes(b"state-\xff.json");
    let same = OsStr::from_bytes(b"state-\xff.json");
    let different = OsStr::from_bytes(b"state-\xfe.json");
    let first_keys = leaf_name_equivalence_keys(first);
    assert!(
        first_keys
            .iter()
            .any(|key| leaf_name_equivalence_keys(same).contains(key))
    );
    assert!(
        !first_keys
            .iter()
            .any(|key| leaf_name_equivalence_keys(different).contains(key))
    );
}

#[test]
fn named_parks_reject_the_same_native_binding_without_registry_effects() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("World")).expect("test directory");
    std::fs::write(temporary.path().join("State.bin"), b"state").expect("test file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");

    let directory = root
        .open_directory(&LeafName::new("World").expect("directory leaf"))
        .expect("directory capability");
    let directory_error = match directory.park_as(LeafName::new("WORLD").expect("directory alias"))
    {
        DirectoryParkOutcome::NoEffect {
            error,
            item: directory,
        } => {
            drop(directory);
            error
        }
        DirectoryParkOutcome::Applied(_) => panic!("same-binding directory park applied"),
        DirectoryParkOutcome::AppliedUnverified(_) => {
            panic!("same-binding directory park became indeterminate")
        }
    };

    let file = root
        .open_file(&LeafName::new("State.bin").expect("file leaf"))
        .expect("file capability");
    let revision = file.revision().expect("file revision");
    let request = file.park_request(ExpectedFileContent::new(revision, [0_u8; 32]));
    let file_error =
        match root.park_file_as(request, LeafName::new("STATE.BIN").expect("file alias")) {
            FileParkOutcome::NoEffect { error, request } => {
                drop(request);
                error
            }
            FileParkOutcome::Parked(_) => panic!("same-binding file park applied"),
            FileParkOutcome::Preserved { .. } => {
                panic!("same-binding file park preserved changed content")
            }
            FileParkOutcome::AppliedUnverified(_) => {
                panic!("same-binding file park became indeterminate")
            }
        };

    let state = session
        .authority
        .operations
        .lock()
        .expect("operation state");
    assert_eq!(state.outstanding_effects, 0);
    assert!(state.file_parks.is_empty());
    assert!(state.directory_parks.is_empty());
    drop(state);
    assert_eq!(directory_error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(file_error.kind(), io::ErrorKind::InvalidInput);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn preserved_file_acknowledgement_leaves_the_leaf_and_clears_ownership() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_preservation_test_file(&root);

    parked.validate_current().expect("validate preserved file");
    {
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert_eq!(state.outstanding_effects, 1);
        assert_eq!(state.file_parks.len(), 1);
        assert!(
            state
                .file_parks
                .values()
                .all(|record| record.cleanup.is_some())
        );
    }

    parked
        .acknowledge_preserved()
        .expect("acknowledge preserved file");

    assert!(!temporary.path().join("record.bin").exists());
    assert_eq!(
        std::fs::read(temporary.path().join("record.preserved")).expect("read preserved file"),
        b"payload",
    );
    let state = session
        .authority
        .operations
        .lock()
        .expect("operation state");
    assert_eq!(state.outstanding_effects, 0);
    assert!(state.file_parks.is_empty());
    drop(state);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn preserved_file_acknowledgement_restores_mismatched_park_authority() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_preservation_test_file(&root);
    {
        let mut state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        let record = state
            .file_parks
            .get_mut(&parked.token.id)
            .expect("registered park");
        record.size = record.size.checked_add(1).expect("mismatched park size");
    }

    assert_eq!(
        parked
            .validate_current()
            .expect_err("mismatched park must fail current validation")
            .kind(),
        io::ErrorKind::InvalidData,
    );

    let error = parked
        .acknowledge_preserved()
        .expect_err("mismatched park must retain ownership");
    assert_eq!(error.error().kind(), io::ErrorKind::InvalidData);
    let parked = error.into_parked();
    assert!(parked.token.armed);
    let state = session
        .authority
        .operations
        .lock()
        .expect("operation state");
    assert_eq!(state.outstanding_effects, 1);
    assert_eq!(state.file_parks.len(), 1);
    assert!(
        state
            .file_parks
            .values()
            .all(|record| record.cleanup.is_some())
    );
    drop(state);

    discard_test_park_registration(parked);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn preserved_file_acknowledgement_returns_mutated_park_authority() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let parked = park_preservation_test_file(&root);
    std::fs::write(
        temporary.path().join("record.preserved"),
        b"mutated-payload",
    )
    .expect("mutate preserved file");

    let error = parked
        .acknowledge_preserved()
        .expect_err("mutated park must retain ownership");
    assert_eq!(error.error().kind(), io::ErrorKind::InvalidData);
    let parked = error.into_parked();
    assert!(parked.token.armed);
    let state = session
        .authority
        .operations
        .lock()
        .expect("operation state");
    assert_eq!(state.outstanding_effects, 1);
    assert_eq!(state.file_parks.len(), 1);
    assert!(
        state
            .file_parks
            .values()
            .all(|record| record.cleanup.is_some())
    );
    drop(state);

    discard_test_park_registration(parked);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn revisions_and_bounded_ranges_are_exact() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("first")).expect("first directory");
    std::fs::create_dir(temporary.path().join("second")).expect("second directory");
    std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let first = root
        .open_directory(&LeafName::new("first").expect("first leaf"))
        .expect("first capability");
    let second = root
        .open_directory(&LeafName::new("second").expect("second leaf"))
        .expect("second capability");
    let directory_revision = first.revision().expect("directory revision");
    first
        .validate_revision(&directory_revision)
        .expect("same directory revision");
    assert_eq!(
        second
            .validate_revision(&directory_revision)
            .expect_err("another directory cannot match the revision")
            .kind(),
        io::ErrorKind::InvalidData,
    );

    let file = root
        .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
        .expect("file capability");
    let revision = file.revision().expect("file revision");
    assert_eq!(revision.size(), 6);
    let _ = revision.modified_at_ns().expect("mtime");
    assert_eq!(
        file.read_range_bounded(&revision, 2, 3)
            .expect("bounded range"),
        b"cde",
    );
    assert!(
        file.read_range_bounded(&revision, 6, 0)
            .expect("empty terminal range")
            .is_empty()
    );
    assert_eq!(
        file.read_range_bounded(&revision, 0, MAX_FILE_RANGE_BYTES + 1)
            .expect_err("oversized range")
            .kind(),
        io::ErrorKind::InvalidInput,
    );
    assert_eq!(
        file.read_range_bounded(&revision, 5, 2)
            .expect_err("range beyond revision")
            .kind(),
        io::ErrorKind::UnexpectedEof,
    );
    assert_eq!(
        file.read_range_bounded(&revision, u64::MAX, 1)
            .expect_err("overflowing range")
            .kind(),
        io::ErrorKind::InvalidInput,
    );
    let mut reader = file
        .into_revision_reader(revision, 6)
        .expect("owned revision reader");
    let mut first = [0_u8; 2];
    reader.read_exact(&mut first).expect("initial read");
    assert_eq!(&first, b"ab");
    assert_eq!(reader.seek(SeekFrom::End(-3)).expect("tail seek"), 3);
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).expect("tail read");
    assert_eq!(tail, b"def");
    assert_eq!(
        reader
            .seek(SeekFrom::Current(1))
            .expect_err("seek beyond revision")
            .kind(),
        io::ErrorKind::InvalidInput,
    );
    let file = reader.finish().expect("stable reader finish");
    drop(file);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn revision_reader_start_failures_retain_every_input() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
    std::fs::write(temporary.path().join("other.bin"), b"other").expect("other file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");

    let sample_name = LeafName::new("sample.bin").expect("sample leaf");
    let file = root.open_file(&sample_name).expect("sample capability");
    let revision = file.revision().expect("sample revision");
    let failure = file
        .into_revision_reader(revision, 5)
        .expect_err("reader bound must reject the revision");
    assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
    let (error, file, revision, max_bytes) = failure.into_parts();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(max_bytes, 5);
    let reader = file
        .into_revision_reader(revision, 6)
        .expect("retained inputs can start a corrected reader");
    let (file, revision) = reader.cancel();
    file.validate_revision(&revision)
        .expect("cancel returns the original capability and revision without proof");
    drop((file, revision));

    let file = root.open_file(&sample_name).expect("sample capability");
    let other = root
        .open_file(&LeafName::new("other.bin").expect("other leaf"))
        .expect("other capability");
    let other_revision = other.revision().expect("other revision");
    let failure = file
        .into_revision_reader(other_revision, 16)
        .expect_err("foreign revision must be rejected");
    assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
    let failure = failure
        .retry()
        .expect_err("foreign revision remains foreign");
    let (_, file, other_revision, max_bytes) = failure.into_parts();
    assert_eq!(max_bytes, 16);
    drop((file, other, other_revision, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn parent_revision_sampling_preserves_membership_and_revocation() {
    let temporary = crate::test_tempdir().unwrap();
    std::fs::create_dir(temporary.path().join("child")).unwrap();
    let session = acquire_test_root(temporary.path());
    let root = session.root().unwrap();
    let child = root
        .open_directory(&LeafName::new("child").unwrap())
        .unwrap();
    let reopened = session.root().unwrap();
    assert_eq!(
        root.parent_revisions_to(&root, 0).unwrap(),
        Some(Vec::new())
    );
    assert!(reopened.parent_revisions_to(&child, 1).is_err());
    assert!(child.parent_revisions_to(&root, 1).is_err());
    assert!(
        root.parent_revisions_to(&child, MAX_REVISION_DEPTH + 1)
            .is_err()
    );
    drop(reopened);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(root.parent_revisions_to(&child, 1).is_err());
}

#[test]
fn parent_revision_sampling_retains_admission_until_joined() {
    let temporary = crate::test_tempdir().unwrap();
    std::fs::create_dir(temporary.path().join("child")).unwrap();
    let session = acquire_test_root(temporary.path());
    let root = session.root().unwrap();
    let descendant = root
        .open_directory(&LeafName::new("child").unwrap())
        .unwrap();
    let (sampled, ready) = std::sync::mpsc::sync_channel(1);
    let (release, resume) = std::sync::mpsc::sync_channel(1);
    *root.authority().unwrap().revision_pause.lock().unwrap() = Some(RevisionPause {
        directory: descendant.inner.identity,
        sampled,
        resume,
    });
    let anchor = root.clone();
    let observing = std::thread::spawn(move || anchor.parent_revisions_to(&descendant, 1));
    let reached = ready.recv_timeout(std::time::Duration::from_secs(5));
    let outcome = session.revoke();
    let _ = release.try_send(());
    let observed = observing.join();
    let refusal = match outcome {
        RootRevokeOutcome::Refused(refusal) => refusal,
        other => panic!("live sampling did not retain revocation: {other:?}"),
    };
    let kind = refusal.error().kind();
    drop(root);
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    reached.unwrap();
    assert_eq!(observed.unwrap().unwrap().unwrap().len(), 1);
    assert_eq!(kind, io::ErrorKind::WouldBlock);
}

#[test]
#[cfg(unix)]
fn parent_revision_sampling_refuses_bindings_displaced_after_sampling() {
    for binding in ["parent", "lease", "root"] {
        let temporary = crate::test_tempdir().unwrap();
        let path = temporary.path().join("root");
        std::fs::create_dir_all(path.join("one/two")).unwrap();
        let session = acquire_test_root(&path);
        let root = session.root().unwrap();
        let descendant = root
            .open_directory(&LeafName::new("one").unwrap())
            .unwrap()
            .open_directory(&LeafName::new("two").unwrap())
            .unwrap();
        let (sampled, ready) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        *root.authority().unwrap().revision_pause.lock().unwrap() = Some(RevisionPause {
            directory: descendant.inner.identity,
            sampled,
            resume,
        });
        let anchor = root.clone();
        let observing = std::thread::spawn(move || anchor.parent_revisions_to(&descendant, 2));
        let reached = ready.recv_timeout(std::time::Duration::from_secs(5));
        let target = match binding {
            "parent" => path.join("one"),
            "lease" => path.join(ROOT_LEASE_NAME),
            "root" => path.clone(),
            _ => unreachable!(),
        };
        let displaced = temporary.path().join("displaced");
        let mut moved = false;
        let mutation = if reached.is_ok() {
            (|| -> io::Result<()> {
                std::fs::rename(&target, &displaced)?;
                moved = true;
                if binding == "lease" {
                    std::fs::write(&target, b"replacement")
                } else {
                    std::fs::create_dir(&target)
                }
            })()
        } else {
            Err(io::Error::other("sample checkpoint was not reached"))
        };
        let _ = release.try_send(());
        let observed = observing.join();
        if moved {
            if target.exists() {
                if binding == "lease" {
                    std::fs::remove_file(&target).unwrap();
                } else {
                    std::fs::remove_dir(&target).unwrap();
                }
            }
            std::fs::rename(&displaced, &target).unwrap();
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        mutation.unwrap();
        assert!(observed.unwrap().is_err(), "accepted displaced {binding}");
    }
}

#[test]
fn parent_revision_sampling_refuses_beyond_its_budget() {
    let temporary = crate::test_tempdir().unwrap();
    std::fs::create_dir_all(temporary.path().join("one/two")).unwrap();
    let session = acquire_test_root(temporary.path());
    let root = session.root().unwrap();
    let descendant = root
        .open_directory(&LeafName::new("one").unwrap())
        .unwrap()
        .open_directory(&LeafName::new("two").unwrap())
        .unwrap();
    let result = root.parent_revisions_to(&descendant, 1);
    drop((descendant, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(result.unwrap().is_none());
}

fn observation_window_fixture(
    temporary: &tempfile::TempDir,
) -> (RootSession, Directory, Directory) {
    std::fs::create_dir_all(temporary.path().join("assets/objects/aa")).unwrap();
    std::fs::create_dir_all(temporary.path().join("other/branch")).unwrap();
    let session = acquire_test_root(temporary.path());
    let root = session.root().unwrap();
    let mut bucket = root.clone();
    for name in ["assets", "objects", "aa"] {
        bucket = bucket
            .open_directory(&LeafName::new(name).unwrap())
            .unwrap();
    }
    (session, root, bucket)
}

#[cfg(unix)]
#[test]
fn observation_window_refuses_ancestry_displaced_after_cached_observation() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    let assets = temporary.path().join("assets");
    let mut cached = None;
    let result = root.observe_validated(|| {
        bucket.identity().unwrap();
        std::fs::rename(assets.join("objects"), assets.join("displaced")).unwrap();
        std::fs::create_dir_all(assets.join("objects/aa")).unwrap();
        cached = Some(bucket.identity().is_ok());
    });
    drop((bucket, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        cached,
        Some(true),
        "Cached observation awaits closing validation"
    );
    assert!(result.is_err(), "Window returned a displaced observation");
}

#[cfg(unix)]
#[test]
fn nested_observation_windows_validate_each_authority_on_close() {
    let first = crate::test_tempdir().unwrap();
    let second = crate::test_tempdir().unwrap();
    let (first_session, first_root, first_bucket) = observation_window_fixture(&first);
    let (second_session, second_root, second_bucket) = observation_window_fixture(&second);
    let assets = second.path().join("assets");
    let mut cached = None;
    let outer = first_root.observe_validated(|| {
        first_bucket.identity().unwrap();
        second_root.observe_validated(|| {
            second_bucket.identity().unwrap();
            std::fs::rename(assets.join("objects"), assets.join("displaced")).unwrap();
            std::fs::create_dir_all(assets.join("objects/aa")).unwrap();
            cached = Some(second_bucket.identity().is_ok());
        })
    });
    drop((first_bucket, first_root, second_bucket, second_root));
    assert!(matches!(first_session.revoke(), RootRevokeOutcome::Revoked));
    assert!(matches!(
        second_session.revoke(),
        RootRevokeOutcome::Revoked
    ));
    assert_eq!(cached, Some(true), "Nested authority shares its own window");
    assert!(
        outer.unwrap().is_err(),
        "Nested window returned a displaced observation"
    );
}

#[cfg(unix)]
#[test]
fn effect_entry_confirms_cached_observations_before_mutating() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    let other = root
        .open_directory(&LeafName::new("other").unwrap())
        .unwrap();
    let assets = temporary.path().join("assets");
    let mut created = None;
    let closed = root.observe_validated(|| {
        bucket.identity().unwrap();
        std::fs::rename(assets.join("objects"), assets.join("displaced")).unwrap();
        std::fs::create_dir_all(assets.join("objects/aa")).unwrap();
        created = Some(other.create_directory(&LeafName::new("created").unwrap()));
    });
    drop((bucket, other, root));
    assert!(closed.is_err());
    assert!(matches!(created, Some(DirectoryCreateOutcome::NoEffect(_))));
    assert!(!temporary.path().join("other/created").exists());
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[cfg(unix)]
#[test]
fn batch_window_reports_ancestry_displaced_after_file_effects() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    let assets = temporary.path().join("assets");
    let mut published = None;
    let result = root.batch_validated(|| {
        bucket.identity().unwrap();
        std::fs::rename(assets.join("objects"), assets.join("displaced")).unwrap();
        std::fs::create_dir_all(assets.join("objects/aa")).unwrap();
        let staged = test_sealed_stage(&bucket, b"payload");
        published = Some(matches!(
            staged.promote_no_replace(&bucket, &bucket, &LeafName::new("file").unwrap()),
            FilePromotionOutcome::Applied(_)
        ));
    });
    drop((bucket, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        published,
        Some(true),
        "Batched effect awaits closing validation"
    );
    assert!(
        result.is_err(),
        "Batch returned effects in a displaced directory"
    );
    assert!(assets.join("displaced/aa/file").is_file());
}

#[test]
fn observation_window_survives_file_effects_and_directory_rebinding() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    let other = root
        .open_directory(&LeafName::new("other").unwrap())
        .unwrap();
    let result = root.observe_validated(|| {
        bucket.identity().unwrap();
        let branch = other
            .open_directory(&LeafName::new("branch").unwrap())
            .unwrap();
        branch.identity().unwrap();
        let staged = test_sealed_stage(&bucket, b"payload");
        let name = LeafName::new("file").unwrap();
        let published = match staged.promote_no_replace(&bucket, &bucket, &name) {
            FilePromotionOutcome::Applied(file) => file,
            _ => panic!("promotion had no exact effect"),
        };
        bucket.identity().unwrap();
        let moved = match branch.move_no_replace(&bucket, &LeafName::new("moved").unwrap()) {
            DirectoryMoveOutcome::Applied(directory) => directory,
            _ => panic!("directory move had no exact effect"),
        };
        moved.identity().unwrap();
        published.revision().unwrap();
    });
    drop((bucket, other, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    result.unwrap();
    assert!(temporary.path().join("assets/objects/aa/moved").is_dir());
}

#[cfg(unix)]
#[test]
fn observation_window_refuses_file_rebound_after_remembered_observation() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    let parent = temporary.path().join("assets/objects/aa");
    std::fs::write(parent.join("file"), b"file").unwrap();
    let file = bucket.open_file(&LeafName::new("file").unwrap()).unwrap();
    let mut cached = None;
    let result = root.observe_validated(|| {
        file.revision().unwrap();
        std::fs::rename(parent.join("file"), parent.join("displaced")).unwrap();
        std::fs::write(parent.join("file"), b"other").unwrap();
        cached = Some(file.revision().is_ok());
    });
    drop((file, bucket, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        cached,
        Some(true),
        "Remembered binding awaits closing validation"
    );
    assert!(
        result.is_err(),
        "Window returned a rebound file observation"
    );
}

#[test]
fn observation_window_remembers_reopened_file_once() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    std::fs::write(temporary.path().join("assets/objects/aa/file"), b"file").unwrap();
    let name = LeafName::new("file").unwrap();
    let authority = root.authority().unwrap();
    let remembered = root
        .observe_validated(|| {
            for _ in 0..=MAX_OBSERVATION_WINDOW_FILES {
                bucket.open_file(&name).unwrap();
            }
            with_observation_window(&authority, |window| window.files.len())
        })
        .unwrap();
    drop((bucket, root, authority));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(remembered, Some(1));
}

#[test]
fn observation_window_refuses_file_linked_after_remembered_observation() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    let parent = temporary.path().join("assets/objects/aa");
    std::fs::write(parent.join("file"), b"file").unwrap();
    let file = bucket.open_file(&LeafName::new("file").unwrap()).unwrap();
    let mut cached = None;
    let result = root.observe_validated(|| {
        file.revision().unwrap();
        std::fs::hard_link(parent.join("file"), parent.join("alias")).unwrap();
        cached = Some(file.same_file(&file).is_ok());
    });
    let unwindowed = file.same_file(&file);
    drop((file, bucket, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert_eq!(
        cached,
        Some(true),
        "Remembered binding awaits closing validation"
    );
    assert!(
        result.is_err(),
        "Window returned a multiply linked file observation"
    );
    assert!(unwindowed.is_err());
}

#[test]
fn effects_forget_remembered_file_bindings() {
    let temporary = crate::test_tempdir().unwrap();
    let (session, root, bucket) = observation_window_fixture(&temporary);
    std::fs::write(temporary.path().join("assets/objects/aa/file"), b"file").unwrap();
    for batch in [false, true] {
        let name = LeafName::new("file").unwrap();
        let file = bucket.open_file(&name).unwrap();
        let moved = LeafName::new("moved").unwrap();
        let work = || {
            file.revision().unwrap();
            let FileMoveOutcome::Applied(file) = file.move_no_replace(&bucket, &moved) else {
                panic!("file move had no exact effect");
            };
            file.revision().unwrap();
            let FileMoveOutcome::Applied(_) = file.move_no_replace(&bucket, &name) else {
                panic!("file move had no exact effect");
            };
        };
        let result = if batch {
            root.batch_validated(work)
        } else {
            root.observe_validated(work)
        };
        result.unwrap();
    }
    drop((bucket, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    assert!(temporary.path().join("assets/objects/aa/file").is_file());
}

fn paused_file_observation(
    root: &Directory,
    name: &LeafName,
    expected: &FileRevisionObservation,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
    std::thread::JoinHandle<io::Result<()>>,
) {
    let (opened, ready) = std::sync::mpsc::sync_channel(1);
    let (release, resume) = std::sync::mpsc::sync_channel(1);
    *root
        .authority()
        .unwrap()
        .file_observation_pause
        .lock()
        .unwrap() = Some(FileObservationPause {
        parent: root.inner.identity,
        name: name.clone(),
        opened,
        resume,
    });
    let root = root.clone();
    let name = name.clone();
    let expected = expected.clone();
    let observing =
        std::thread::spawn(move || root.validate_file_revision_observation(&name, &expected));
    (ready, release, observing)
}

fn observation_fixture(
    external: bool,
) -> (
    tempfile::TempDir,
    RootSession,
    Directory,
    std::path::PathBuf,
) {
    let temporary = crate::test_tempdir().unwrap();
    let app = temporary.path().join("app");
    std::fs::create_dir(&app).unwrap();
    let session = acquire_test_root(&app);
    let (directory, path) = if external {
        let library = temporary.path().join("outer/library");
        std::fs::create_dir_all(&library).unwrap();
        (session.admit_absolute_directory(&library).unwrap(), library)
    } else {
        (session.root().unwrap(), app)
    };
    std::fs::write(path.join("sample.bin"), b"sample").unwrap();
    (temporary, session, directory, path)
}

#[cfg(unix)]
#[test]
fn file_revision_observation_refuses_displaced_bindings_after_open() {
    for (external, binding) in [
        (false, "file"),
        (false, "lease"),
        (false, "root"),
        (true, "file"),
        (true, "lease"),
        (true, "root"),
        (true, "library"),
        (true, "ancestor"),
    ] {
        let (temporary, session, root, root_path) = observation_fixture(external);
        let name = LeafName::new("sample.bin").unwrap();
        let file = root.open_file(&name).unwrap();
        let expected = file.revision().unwrap().observation();
        let (ready, release, observing) = paused_file_observation(&root, &name, &expected);
        let opened = ready.recv_timeout(std::time::Duration::from_secs(5));
        let path = match binding {
            "file" => root_path.join("sample.bin"),
            "lease" => temporary.path().join("app").join(ROOT_LEASE_NAME),
            "root" => temporary.path().join("app"),
            "library" => root_path.clone(),
            "ancestor" => root_path.parent().unwrap().to_path_buf(),
            _ => unreachable!(),
        };
        let displaced = temporary.path().join("displaced");
        let directory_binding = path.is_dir();
        let mut moved = false;
        let mutation = if opened.is_ok() {
            (|| -> io::Result<()> {
                std::fs::rename(&path, &displaced)?;
                moved = true;
                if directory_binding {
                    std::fs::create_dir(&path)
                } else {
                    std::fs::write(&path, b"sample")
                }
            })()
        } else {
            Err(io::Error::other("observation did not reach native open"))
        };
        let _ = release.try_send(());
        let observed = observing.join();
        if moved {
            if path.exists() {
                if directory_binding {
                    std::fs::remove_dir(&path).unwrap();
                } else {
                    std::fs::remove_file(&path).unwrap();
                }
            }
            std::fs::rename(&displaced, &path).unwrap();
        }
        let preserved = std::fs::read(root_path.join("sample.bin"));
        drop((file, expected, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        mutation.unwrap();
        assert!(observed.unwrap().is_err(), "accepted displaced {binding}");
        assert_eq!(preserved.unwrap(), b"sample");
    }
}

#[test]
fn file_revision_observation_refuses_original_proof_after_file_changes() {
    for (external, change) in [false, true].into_iter().flat_map(|external| {
        [
            "replacement",
            "missing",
            "hardlink",
            "directory",
            "restored bytes",
        ]
        .map(|change| (external, change))
    }) {
        let (temporary, session, root, root_path) = observation_fixture(external);
        let path = root_path.join("sample.bin");
        let name = LeafName::new("sample.bin").unwrap();
        let file = root.open_file(&name).unwrap();
        let expected = file.revision().unwrap().observation();
        match change {
            "replacement" => {
                std::fs::rename(&path, temporary.path().join("previous")).unwrap();
                std::fs::write(&path, b"sample").unwrap();
            }
            "missing" => std::fs::remove_file(&path).unwrap(),
            "hardlink" => std::fs::hard_link(&path, temporary.path().join("alias")).unwrap(),
            "directory" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "restored bytes" => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    std::fs::write(&path, b"change").unwrap();
                    if file.revision().unwrap().observation() != expected {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "file stamp did not change"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                std::fs::write(&path, b"sample").unwrap();
                assert!(file.revision().unwrap().observation() != expected);
            }
            _ => unreachable!(),
        }
        let observed = root.validate_file_revision_observation(&name, &expected);
        drop((file, expected, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        assert!(observed.is_err(), "accepted original proof after {change}");
    }
}

#[test]
fn file_revision_observation_preserves_authority_and_reserved_leaf_refusal() {
    let temporary = crate::test_tempdir().unwrap();
    let path = temporary.path().join("sample.bin");
    std::fs::write(&path, b"sample").unwrap();
    let session = acquire_test_root(temporary.path());
    let root = session.root().unwrap();
    let name = LeafName::new("sample.bin").unwrap();
    let file = root.open_file(&name).unwrap();
    let expected = file.revision().unwrap().observation();
    let foreign_temporary = crate::test_tempdir().unwrap();
    std::fs::write(foreign_temporary.path().join("sample.bin"), b"sample").unwrap();
    let foreign_session = acquire_test_root(foreign_temporary.path());
    let foreign = foreign_session.root().unwrap();
    let foreign_result = foreign.validate_file_revision_observation(&name, &expected);
    drop(foreign);
    assert!(matches!(
        foreign_session.revoke(),
        RootRevokeOutcome::Revoked
    ));

    let control = root
        .validate_file_revision_observation(&LeafName::new(ROOT_LEASE_NAME).unwrap(), &expected);
    std::fs::rename(&path, temporary.path().join("previous")).unwrap();
    let destination = root.admit_transient_destination(name.clone()).unwrap();
    std::fs::write(&path, b"sample").unwrap();
    let reserved = root.validate_file_revision_observation(&name, &expected);
    let cancelled = destination.cancel();
    let fresh = root
        .open_file(&name)
        .unwrap()
        .revision()
        .unwrap()
        .observation();
    let available = root.validate_file_revision_observation(&name, &fresh);
    drop(file);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    let retired = root.validate_file_revision_observation(&name, &fresh);
    assert!(foreign_result.is_err());
    assert_eq!(control.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(reserved.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(matches!(
        cancelled,
        TransientDestinationCancelOutcome::Cancelled
    ));
    available.unwrap();
    assert!(retired.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"sample");
    assert_eq!(
        std::fs::read(temporary.path().join("previous")).unwrap(),
        b"sample"
    );
}

#[test]
fn file_revision_observation_holds_admission_until_joined() {
    for external in [false, true] {
        let (_temporary, session, root, root_path) = observation_fixture(external);
        let name = LeafName::new("sample.bin").unwrap();
        let expected = root
            .open_file(&name)
            .unwrap()
            .revision()
            .unwrap()
            .observation();
        let (ready, release, observing) = paused_file_observation(&root, &name, &expected);
        let opened = ready.recv_timeout(std::time::Duration::from_secs(5));
        let outcome = session.revoke();
        let _ = release.try_send(());
        let observed = observing.join();
        let refusal = match outcome {
            RootRevokeOutcome::Refused(refusal) => refusal,
            other => panic!("live observation did not retain revocation: {other:?}"),
        };
        let kind = refusal.error().kind();
        let retired = root.clone();
        drop(root);
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
        opened.unwrap();
        observed.unwrap().unwrap();
        assert_eq!(kind, io::ErrorKind::WouldBlock);
        assert!(
            retired
                .validate_file_revision_observation(&name, &expected)
                .is_err()
        );
        assert_eq!(
            std::fs::read(root_path.join("sample.bin")).unwrap(),
            b"sample"
        );
    }
}

#[test]
fn file_revision_observation_blocks_reset_until_joined() {
    for external in [false, true] {
        let (_temporary, session, root, root_path) = observation_fixture(external);
        let name = LeafName::new("sample.bin").unwrap();
        let expected = root
            .open_file(&name)
            .unwrap()
            .revision()
            .unwrap()
            .observation();
        let (ready, release, observing) = paused_file_observation(&root, &name, &expected);
        let opened = ready.recv_timeout(std::time::Duration::from_secs(5));
        let outcome = session.begin_reset();
        let _ = release.try_send(());
        let observed = observing.join();
        let refusal = match outcome {
            ResetStartOutcome::Refused(refusal) => refusal,
            other => panic!("live observation did not retain reset: {other:?}"),
        };
        let kind = refusal.error().kind();
        let session = refusal.cancel_reset();
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        opened.unwrap();
        observed.unwrap().unwrap();
        assert_eq!(kind, io::ErrorKind::WouldBlock);
        assert_eq!(
            std::fs::read(root_path.join("sample.bin")).unwrap(),
            b"sample"
        );
    }
}

#[test]
fn revision_reader_operation_blocks_revocation_until_finish() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root
        .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
        .expect("sample capability");
    let revision = file.revision().expect("sample revision");
    let reader = file
        .into_revision_reader(revision, 6)
        .expect("owned revision reader");

    let refusal = match session.revoke() {
        RootRevokeOutcome::Refused(failure) => failure,
        outcome => panic!("live reader operation did not block revocation: {outcome:?}"),
    };
    assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
    let file = reader.finish().expect("stable reader finish");
    drop((file, root));
    assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
}

#[test]
fn revision_reader_operation_blocks_reset_until_cancel() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root
        .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
        .expect("sample capability");
    let revision = file.revision().expect("sample revision");
    let reader = file
        .into_revision_reader(revision, 6)
        .expect("owned revision reader");

    let refusal = match session.begin_reset() {
        ResetStartOutcome::Refused(failure) => failure,
        outcome => panic!("live reader operation did not block reset: {outcome:?}"),
    };
    assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
    let (file, revision) = reader.cancel();
    let session = refusal.cancel_reset();
    drop((file, revision, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn revision_reader_finish_failure_retains_the_reader() {
    let temporary = crate::test_tempdir().expect("temporary root");
    let path = temporary.path().join("sample.bin");
    std::fs::write(&path, b"abcdef").expect("sample file");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let file = root
        .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
        .expect("sample capability");
    let revision = file.revision().expect("sample revision");
    let reader = file
        .into_revision_reader(revision, 6)
        .expect("owned revision reader");
    std::fs::write(&path, b"changed").expect("mutate admitted file");

    let failure = reader
        .finish()
        .expect_err("changed revision must fail final settlement");
    assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
    let reader = failure.into_reader();
    let (file, revision) = reader.cancel();
    assert_eq!(
        file.validate_revision(&revision)
            .expect_err("cancel does not claim a stable revision")
            .kind(),
        io::ErrorKind::InvalidData,
    );
    drop((file, revision, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}

#[test]
fn stable_park_header_reserves_names_during_file_record_checkout() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::write(temporary.path().join("file.parked"), b"payload").expect("parked file");
    std::fs::create_dir(temporary.path().join("directory.parked")).expect("parked directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");

    let file = root
        .open_file(&LeafName::new("file.parked").expect("file park leaf"))
        .expect("parked file capability");
    let revision = file.revision().expect("file revision");
    let digest: [u8; 32] = Sha256::digest(b"payload").into();
    let request = file.park_request(ExpectedFileContent::new(revision, digest));
    let mut parked_file = root
        .admit_existing_file_park(
            &LeafName::new("file.original").expect("file original leaf"),
            request,
        )
        .expect("existing file park admission");

    let directory = root
        .open_directory(&LeafName::new("directory.parked").expect("directory park leaf"))
        .expect("parked directory capability");
    let directory_revision = directory.revision().expect("directory revision");
    let conflicting_original = LeafName::new("FILE.ORIGINAL").expect("aliasing original leaf");
    let conflicting_park = LeafName::new("directory.parked").expect("directory park leaf");

    let authority = parked_file.authority().expect("park authority");
    let operation = authority
        .enter_file_park(&parked_file.token)
        .expect("file park operation");
    let guard = authority
        .take_file_park(&operation, &parked_file.token)
        .expect("checked-out file park");
    let mut unrelated = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("unrelated-source").expect("unrelated source"),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("unrelated-destination").expect("unrelated destination"),
        },
        None,
        None,
    )
    .expect("checked-out park must not block an unrelated sibling");
    unrelated
        .settle(&operation)
        .expect("settle unrelated move reservation");
    let checked_out_conflict = authority
        .ensure_park_available(
            &operation,
            &root,
            &conflicting_original,
            &conflicting_park,
            Some(directory.inner.identity.physical),
        )
        .map_err(|error| error.kind());
    drop(guard);
    drop(operation);

    let admission_error = root
        .admit_existing_directory_park(
            &LeafName::new("FILE.ORIGINAL").expect("aliasing original leaf"),
            directory,
            &directory_revision,
        )
        .expect_err("cross-kind alias must remain owned")
        .kind();

    let operation = authority
        .enter_file_park(&parked_file.token)
        .expect("file park cleanup operation");
    let guard = authority
        .take_file_park(&operation, &parked_file.token)
        .expect("file park cleanup guard");
    guard.disarm(&mut parked_file.token, &operation);
    drop(operation);
    drop(parked_file);
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));

    assert_eq!(checked_out_conflict, Err(io::ErrorKind::AlreadyExists));
    assert_eq!(admission_error, io::ErrorKind::AlreadyExists);
}

#[test]
fn stable_headers_reserve_checked_out_creates_and_directory_parks() {
    let temporary = crate::test_tempdir().expect("temporary root");
    std::fs::create_dir(temporary.path().join("folder")).expect("parked directory");
    let session = acquire_test_root(temporary.path());
    let root = session.root().expect("root capability");
    let authority = root.authority().expect("root authority");
    let operation = authority.enter().expect("create reservation operation");

    let stage_name = LeafName::new("stage-create").expect("stage create leaf");
    let mut stage_create = authority
        .reserve_stage_create(&operation, &root, &stage_name, None)
        .expect("stage create reservation");
    let stage_guard = authority
        .take_stage_create(&operation, &stage_create)
        .expect("checked-out stage create");
    let stage_conflict = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: stage_name,
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("stage-conflict-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect_err("checked-out stage create must retain its leaf");
    assert_eq!(stage_conflict.kind(), io::ErrorKind::WouldBlock);
    let mut unrelated = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("stage-unrelated-source").expect("source"),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("stage-unrelated-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect("checked-out stage create must not block a sibling");
    unrelated.settle(&operation).expect("settle sibling move");
    stage_guard
        .disarm(&mut stage_create, &operation)
        .expect("checked-out stage create settles");

    let directory_name = LeafName::new("directory-create").expect("directory create leaf");
    let mut directory_create = authority
        .reserve_directory_create(&operation, &root, &directory_name)
        .expect("directory create reservation");
    let directory_guard = authority
        .take_directory_create(&operation, &directory_create)
        .expect("checked-out directory create");
    let directory_conflict = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: directory_name,
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("directory-conflict-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect_err("checked-out directory create must retain its leaf");
    assert_eq!(directory_conflict.kind(), io::ErrorKind::WouldBlock);
    let mut unrelated = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("directory-unrelated-source").expect("source"),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("directory-unrelated-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect("identity-unknown directory create must permit an unrelated sibling");
    unrelated.settle(&operation).expect("settle sibling move");
    directory_guard.disarm(&mut directory_create, &operation);
    let mut unrelated = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("directory-unrelated-source").expect("source"),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("directory-unrelated-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect("settled directory create releases unrelated siblings");
    unrelated.settle(&operation).expect("settle sibling move");
    drop(operation);

    let parked = park_test_directory(&root, "folder", "folder.park");
    let operation = authority
        .enter_directory_park(&parked.token)
        .expect("directory park operation");
    let guard = authority
        .take_directory_park(&operation, &parked.token)
        .expect("checked-out directory park");
    let park_conflict = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("folder").expect("original leaf"),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("park-conflict-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect_err("checked-out directory park must retain its original leaf");
    assert_eq!(park_conflict.kind(), io::ErrorKind::WouldBlock);
    let mut unrelated = MoveEffectToken::reserve(
        &authority,
        &operation,
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("park-unrelated-source").expect("source"),
        },
        NamespaceLeaf {
            parent: root.clone(),
            name: LeafName::new("park-unrelated-destination").expect("destination"),
        },
        None,
        None,
    )
    .expect("checked-out directory park must not block a sibling");
    unrelated.settle(&operation).expect("settle sibling move");
    drop(guard);
    drop(operation);
    let restored = match parked.restore() {
        DirectoryRestoreOutcome::Applied(directory) => directory,
        DirectoryRestoreOutcome::NoEffect { error, .. } => {
            panic!("directory restoration had no effect: {error}")
        }
        DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
            panic!(
                "directory restoration was indeterminate: {}",
                obligation.error()
            )
        }
    };

    drop((restored, authority, root));
    assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
}
