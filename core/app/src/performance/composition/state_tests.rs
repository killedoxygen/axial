use super::*;
use crate::performance::composition::storage::TestManagedStorage;
use crate::performance::model::{
    ManagedArtifactIntegrity, ManagedArtifactProvider, ManagedArtifactRole, ManagedArtifactSource,
    ManagedDependencyStateEdge, VersionFamily,
};
use std::fs;

#[test]
fn duplicate_payload_restores_independent_artifacts_with_current_or_history_only_state() {
    for current_present in [true, false] {
        let root = test_root("duplicate-rollback");
        let source_path = root.join("source");
        let destination_path = root.join("destination");
        fs::create_dir_all(source_path.join("mods")).unwrap();
        fs::create_dir_all(destination_path.join("mods")).unwrap();
        let storage = TestManagedStorage::new(&root);
        let source = storage.directory().open_child("source").unwrap().unwrap();
        let source_mods = source.open_child("mods").unwrap().unwrap();
        let old_state = test_state(vec![test_installed("managed.jar", b"old managed bytes")]);
        fs::write(source_path.join("mods/managed.jar"), b"old managed bytes").unwrap();
        save_state(&source_mods, &old_state).unwrap();
        let absent = save_absent_rollback_snapshot(&source_mods).unwrap();
        let snapshot = save_rollback_snapshot(&source_mods, &old_state).unwrap();
        if current_present {
            fs::write(
                source_path.join("mods/managed.jar"),
                b"current managed bytes",
            )
            .unwrap();
            save_state(
                &source_mods,
                &test_state(vec![test_installed(
                    "managed.jar",
                    b"current managed bytes",
                )]),
            )
            .unwrap();
            fs::copy(
                source_path.join("mods/managed.jar"),
                destination_path.join("mods/managed.jar"),
            )
            .unwrap();
        } else {
            let artifact = &old_state.installed_mods[0];
            stage_managed_artifact_removal(&source_mods, artifact).unwrap();
            remove_state(&source_mods).unwrap();
            settle_managed_artifact_removal(&source_mods, artifact).unwrap();
            recover_managed_storage(&source_mods).unwrap();
        }
        fs::write(source_path.join("mods/user.jar"), b"unmanaged bytes").unwrap();
        fs::copy(
            source_path.join("mods/user.jar"),
            destination_path.join("mods/user.jar"),
        )
        .unwrap();
        let payload = ManagedDuplicatePayload::admit(source.directory()).unwrap();
        copy_duplicate_fixture(&payload, &source_path, &destination_path);
        let destination = storage
            .directory()
            .open_child("destination")
            .unwrap()
            .unwrap();
        payload.verify_staged(destination.directory()).unwrap();
        let effects = ManagedInstanceEffectAuthority::bind(destination.directory()).unwrap();
        let destination =
            ManagedStorageDirectory::bind_instance_root(destination.directory().clone(), effects)
                .unwrap();
        let destination_mods = destination.open_child("mods").unwrap().unwrap();
        let summaries = list_rollback_snapshots_admitted(&destination_mods).unwrap();
        assert!(summaries.iter().any(|entry| entry.id == absent.id
            && entry.target == RollbackSnapshotTarget::ManagedStateAbsent));
        assert!(summaries.iter().any(|entry| entry.id == snapshot.id));
        let copied_snapshot =
            load_rollback_snapshot_by_id_admitted(&destination_mods, &snapshot.id)
                .unwrap()
                .unwrap();
        restore_rollback_snapshot(&destination_mods, &copied_snapshot).unwrap();
        assert_eq!(
            fs::read(destination_path.join("mods/managed.jar")).unwrap(),
            b"old managed bytes"
        );
        assert_eq!(
            load_state_admitted(&destination_mods).unwrap(),
            Some(old_state)
        );
        assert_eq!(
            fs::read(destination_path.join("mods/user.jar")).unwrap(),
            b"unmanaged bytes"
        );
        if current_present {
            assert_eq!(
                fs::read(source_path.join("mods/managed.jar")).unwrap(),
                b"current managed bytes"
            );
        } else {
            assert!(!source_path.join("mods/managed.jar").exists());
            assert!(load_state_admitted(&source_mods).unwrap().is_none());
        }
        assert_eq!(
            load_rollback_snapshot_by_id_admitted(&source_mods, &snapshot.id).unwrap(),
            Some(snapshot)
        );
    }
}

#[test]
fn duplicate_payload_refuses_unknown_and_unsettled_records_without_recovery() {
    for residue in [
        ".axial-lock.json.delete.intent",
        ".axial-lock.json.previous.tmp",
        ".AXIAL-LOCK.JSON",
        ".axial-performance/unknown",
        ".axial-performance/mutations/unknown",
        ".axial-performance/mutations/candidates/unfinished",
        ".axial-performance/quarantine/digest/parked",
        ".axial-performance/rollback/tmp/candidate-unfinished/snapshot.json",
        ".axial-performance/rollback/unknown",
    ] {
        let root = test_root("duplicate-refusal");
        let residue_path = root.join("mods").join(residue);
        fs::create_dir_all(residue_path.parent().unwrap()).unwrap();
        fs::write(&residue_path, b"preserve exact residue").unwrap();
        let storage = TestManagedStorage::new(&root);
        assert!(
            ManagedDuplicatePayload::admit(storage.directory().directory()).is_err(),
            "{residue}"
        );
        assert_eq!(fs::read(&residue_path).unwrap(), b"preserve exact residue");
    }
}

#[test]
fn duplicate_payload_rechecks_current_and_historical_bytes_before_publication() {
    let root = test_root("duplicate-drift");
    let source_path = root.join("source");
    let destination_path = root.join("destination");
    fs::create_dir_all(source_path.join("mods")).unwrap();
    fs::create_dir_all(destination_path.join("mods")).unwrap();
    let storage = TestManagedStorage::new(&root);
    let source = storage.directory().open_child("source").unwrap().unwrap();
    let mods = source.open_child("mods").unwrap().unwrap();
    let state = test_state(vec![test_installed("managed.jar", b"managed bytes")]);
    fs::write(source_path.join("mods/managed.jar"), b"managed bytes").unwrap();
    save_state(&mods, &state).unwrap();
    let snapshot = save_rollback_snapshot(&mods, &state).unwrap();
    let payload = ManagedDuplicatePayload::admit(source.directory()).unwrap();
    copy_duplicate_fixture(&payload, &source_path, &destination_path);
    fs::copy(
        source_path.join("mods/managed.jar"),
        destination_path.join("mods/managed.jar"),
    )
    .unwrap();
    let destination = storage
        .directory()
        .open_child("destination")
        .unwrap()
        .unwrap();
    payload.verify_staged(destination.directory()).unwrap();
    fs::write(source_path.join("mods/managed.jar"), b"changed bytes").unwrap();
    assert!(payload.verify_staged(destination.directory()).is_err());
    fs::write(source_path.join("mods/managed.jar"), b"managed bytes").unwrap();
    fs::write(
        rollback_history_path(&destination_path.join("mods"))
            .join(&snapshot.id)
            .join("artifact-000.bin"),
        b"corrupt history",
    )
    .unwrap();
    assert!(payload.verify_staged(destination.directory()).is_err());
    assert_eq!(
        fs::read(source_path.join("mods/managed.jar")).unwrap(),
        b"managed bytes"
    );
}

#[test]
fn duplicate_payload_rejects_metadata_changed_between_parse_and_fingerprint() {
    for history in [false, true] {
        let root = test_root("duplicate-metadata-race");
        fs::create_dir_all(root.join("mods")).unwrap();
        let storage = TestManagedStorage::new(&root);
        let mods = storage.directory().open_child("mods").unwrap().unwrap();
        let state = test_state(Vec::new());
        save_state(&mods, &state).unwrap();
        let path = if history {
            let snapshot = save_rollback_snapshot(&mods, &state).unwrap();
            fs::remove_file(root.join("mods").join(LOCK_FILE_NAME)).unwrap();
            rollback_history_path(&root.join("mods"))
                .join(snapshot.id)
                .join(ROLLBACK_METADATA_FILE_NAME)
        } else {
            root.join("mods").join(LOCK_FILE_NAME)
        };
        let mut changed = fs::read(&path).unwrap();
        changed.push(b'\n');
        let replacement = changed.clone();
        let target = path.clone();
        DUPLICATE_METADATA_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || fs::write(target, replacement).unwrap()));
        });
        assert!(ManagedDuplicatePayload::admit(storage.directory().directory()).is_err());
        assert_eq!(fs::read(path).unwrap(), changed);
    }
}

#[test]
fn duplicate_recovery_rechecks_witness_and_current_artifacts_without_the_source() {
    let root = test_root("duplicate-recovery-witness");
    let source_path = root.join("source");
    let destination_path = root.join("destination");
    fs::create_dir_all(source_path.join("mods")).unwrap();
    fs::create_dir_all(destination_path.join("mods")).unwrap();
    let storage = TestManagedStorage::new(&root);
    let source = storage.directory().open_child("source").unwrap().unwrap();
    let source_mods = source.open_child("mods").unwrap().unwrap();
    let state = test_state(vec![test_installed(
        "managed.jar",
        b"original managed bytes",
    )]);
    fs::write(
        source_path.join("mods/managed.jar"),
        b"original managed bytes",
    )
    .unwrap();
    save_state(&source_mods, &state).unwrap();
    let snapshot = save_rollback_snapshot(&source_mods, &state).unwrap();
    let payload = ManagedDuplicatePayload::admit(source.directory()).unwrap();
    let witness = payload.witness();
    assert!(witness.len() < 256);
    copy_duplicate_fixture(&payload, &source_path, &destination_path);
    fs::copy(
        source_path.join("mods/managed.jar"),
        destination_path.join("mods/managed.jar"),
    )
    .unwrap();
    drop(payload);
    fs::rename(&source_path, root.join("source-moved")).unwrap();
    let destination = storage
        .directory()
        .open_child("destination")
        .unwrap()
        .unwrap();
    ManagedDuplicatePayload::verify_recovered(destination.directory(), Some(&witness)).unwrap();
    assert!(ManagedDuplicatePayload::verify_recovered(destination.directory(), None).is_err());
    fs::write(
        destination_path.join("mods/managed.jar"),
        b"changed managed bytes",
    )
    .unwrap();
    assert!(
        ManagedDuplicatePayload::verify_recovered(destination.directory(), Some(&witness)).is_err()
    );
    fs::write(
        destination_path.join("mods/managed.jar"),
        b"original managed bytes",
    )
    .unwrap();
    let metadata = rollback_history_path(&destination_path.join("mods"))
        .join(&snapshot.id)
        .join(ROLLBACK_METADATA_FILE_NAME);
    let original = fs::read(&metadata).unwrap();
    let mut changed = original.clone();
    changed.push(b'\n');
    fs::write(&metadata, changed).unwrap();
    assert!(
        ManagedDuplicatePayload::verify_recovered(destination.directory(), Some(&witness)).is_err()
    );
    fs::write(&metadata, original).unwrap();
    ManagedDuplicatePayload::verify_recovered(destination.directory(), Some(&witness)).unwrap();
    fs::remove_file(destination_path.join("mods/.axial-lock.json")).unwrap();
    assert!(
        ManagedDuplicatePayload::verify_recovered(destination.directory(), Some(&witness)).is_err()
    );
}

fn copy_duplicate_fixture(payload: &ManagedDuplicatePayload, source: &Path, destination: &Path) {
    for file in payload.files() {
        let relative = file.relative_components().iter().collect::<PathBuf>();
        let target = destination.join(&relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(source.join(relative), target).unwrap();
    }
}

#[test]
fn pre_ready_candidate_recovery_preserves_an_existing_final_file() {
    let root = test_root("candidate-pre-ready-recovery");
    fs::create_dir_all(&root).expect("create root");
    let bytes = b"managed-candidate";
    let installed = test_installed("managed.jar", bytes);
    fs::write(root.join(&installed.filename), bytes).expect("write existing final file");
    let storage = TestManagedStorage::new(&root);
    let intent = prepare_managed_artifact_candidate(storage.directory(), &installed)
        .expect("prepare candidate intent");
    drop(
        intent
            .candidate_parent()
            .create_file_create_new(Path::new(intent.candidate_name()), bytes)
            .expect("create exact candidate"),
    );

    reconcile_managed_candidate_intents(storage.directory()).expect("rollback pre-ready candidate");

    assert_eq!(
        fs::read(root.join(&installed.filename)).expect("read preserved final file"),
        bytes
    );
    assert!(
        !root
            .join(candidate_relative(&candidate_name(&installed)))
            .exists()
    );
    assert!(
        !root
            .join(candidate_intent_relative(&addition_marker_name(&installed)))
            .exists()
    );
    prove_managed_storage_recovered(storage.directory(), None)
        .expect("candidate rollback leaves recovered storage");
}

#[test]
fn ready_candidate_settlement_keeps_the_tracked_final_and_cleans_internal_bytes() {
    let root = test_root("candidate-ready-settlement");
    fs::create_dir_all(&root).expect("create root");
    let bytes = b"managed-candidate";
    let installed = test_installed("managed.jar", bytes);
    let state = test_state(vec![installed.clone()]);
    let storage = TestManagedStorage::new(&root);
    let intent = prepare_managed_artifact_candidate(storage.directory(), &installed)
        .expect("prepare candidate intent");
    drop(
        intent
            .candidate_parent()
            .create_file_create_new(Path::new(intent.candidate_name()), bytes)
            .expect("create exact candidate"),
    );
    let addition = prepare_managed_artifact_addition(storage.directory(), &installed)
        .expect("prepare ready marker");
    drop(
        storage
            .directory()
            .create_file_create_new(Path::new(&installed.filename), bytes)
            .expect("publish final file"),
    );
    publish_managed_artifact_addition(storage.directory(), &installed, &addition)
        .expect("prove ready publication");
    save_state(storage.directory(), &state).expect("publish tracked state");

    reconcile_managed_addition_obligations(storage.directory(), Some(&state))
        .expect("settle ready candidate");

    assert_eq!(
        fs::read(root.join(&installed.filename)).expect("read tracked final file"),
        bytes
    );
    assert!(
        !root
            .join(candidate_relative(&candidate_name(&installed)))
            .exists()
    );
    assert!(
        !root
            .join(candidate_intent_relative(&addition_marker_name(&installed)))
            .exists()
    );
    prove_managed_storage_recovered(storage.directory(), Some(&state))
        .expect("ready settlement leaves recovered storage");
}

#[test]
fn state_replacement_refuses_competing_recovery_without_residue() {
    use axial_fs::{FileCreateOutcome, LeafName, StageDiscardOutcome, StageDiscardResolution};

    let root = test_root("state-replacement-contention");
    fs::create_dir_all(&root).expect("create root");
    let storage = TestManagedStorage::new(&root);
    let original = test_state(Vec::new());
    save_state(storage.directory(), &original).expect("save original state");
    let blocker = match storage
        .directory()
        .directory()
        .create_recoverable_stage(&LeafName::new("blocked.json").expect("blocker destination"))
    {
        FileCreateOutcome::Created(staged) => staged,
        outcome => panic!("create recovery blocker: {outcome:?}"),
    };
    let mut replacement = original.clone();
    replacement.composition_id = "replacement".to_string();
    replacement.graph_sha512 =
        crate::performance::composition::plan::canonical_state_graph_digest(&replacement)
            .expect("replacement graph digest");
    assert!(save_state(storage.directory(), &replacement).is_err());
    assert_eq!(
        load_state_admitted(storage.directory()).expect("load original state"),
        Some(original)
    );
    assert!(!root.join(".axial-lock.json.new.tmp").exists());
    assert!(!root.join(".axial-lock.json.previous.tmp").exists());
    match blocker.discard() {
        StageDiscardOutcome::Discarded => {}
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            assert!(matches!(
                obligation.reconcile(),
                StageDiscardResolution::Discarded
            ));
        }
    }
    save_state(storage.directory(), &replacement).expect("retry replacement state");
    assert_eq!(
        load_state(storage.directory()).expect("load replacement state"),
        Some(replacement)
    );
}

#[test]
fn rollback_snapshot_restores_managed_bytes_and_state() {
    let root = test_root("rollback-restore");
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("managed.jar"), b"managed-v1").expect("write managed artifact");
    let storage = TestManagedStorage::new(&root);
    let installed = test_installed("managed.jar", b"managed-v1");
    let state = test_state(vec![installed.clone()]);
    save_state(storage.directory(), &state).expect("save state");
    let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");

    stage_managed_artifact_removal(storage.directory(), &installed).expect("stage removal");
    remove_state(storage.directory()).expect("remove state");
    settle_managed_artifact_removal(storage.directory(), &installed).expect("settle removal");
    restore_rollback_snapshot(storage.directory(), &snapshot).expect("restore snapshot");

    assert_eq!(
        fs::read(root.join("managed.jar")).expect("read restored"),
        b"managed-v1"
    );
    assert_eq!(
        load_state(storage.directory()).expect("load restored"),
        Some(state)
    );
}

#[test]
fn rollback_restore_rejects_unowned_destination_before_effects() {
    let root = test_root("rollback-unowned-destination");
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("first.jar"), b"managed-first").expect("write first artifact");
    fs::write(root.join("blocked.jar"), b"managed-blocked").expect("write blocked artifact");
    let storage = TestManagedStorage::new(&root);
    let first = test_installed_identity("first.jar", b"managed-first", "AANobbMI", "NFkjnzWE");
    let blocked =
        test_installed_identity("blocked.jar", b"managed-blocked", "BBNobbMI", "OFkjnzWE");
    let state = test_state(vec![first.clone(), blocked.clone()]);
    save_state(storage.directory(), &state).expect("save state");
    let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");

    stage_managed_artifact_removal(storage.directory(), &first).expect("stage first removal");
    stage_managed_artifact_removal(storage.directory(), &blocked).expect("stage blocked removal");
    remove_state(storage.directory()).expect("remove state");
    settle_managed_artifact_removal(storage.directory(), &first).expect("settle first removal");
    settle_managed_artifact_removal(storage.directory(), &blocked).expect("settle blocked removal");
    fs::write(root.join("blocked.jar"), b"user-owned").expect("write unowned destination");

    let result = restore_rollback_snapshot_classified(storage.directory(), &snapshot);

    assert!(matches!(
        result,
        Err(RollbackRestoreError::Definite(StateError::InvalidRollback(
            _
        )))
    ));
    assert!(!root.join("first.jar").exists());
    assert_eq!(
        fs::read(root.join("blocked.jar")).expect("read unowned destination"),
        b"user-owned"
    );
    assert_eq!(
        load_state(storage.directory()).expect("load unchanged state"),
        None
    );
    prove_managed_storage_recovered(storage.directory(), None)
        .expect("preflight rejection leaves no mutation obligation");
}

#[test]
fn rollback_restore_replaces_case_only_managed_identity() {
    let root = test_root("rollback-case-only-managed-identity");
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("managed.jar"), b"snapshot-managed").expect("write snapshot artifact");
    let storage = TestManagedStorage::new(&root);
    let snapshot_artifact = test_installed("managed.jar", b"snapshot-managed");
    let snapshot_state = test_state(vec![snapshot_artifact.clone()]);
    save_state(storage.directory(), &snapshot_state).expect("save snapshot state");
    let snapshot = save_rollback_snapshot(storage.directory(), &snapshot_state).expect("snapshot");

    let current_artifact = test_installed("Managed.jar", b"current-managed");
    let current_state = test_state(vec![current_artifact.clone()]);
    stage_managed_artifact_removal(storage.directory(), &snapshot_artifact)
        .expect("stage snapshot artifact removal");
    let addition = prepare_managed_artifact_addition(storage.directory(), &current_artifact)
        .expect("prepare current artifact");
    storage
        .directory()
        .create_file_create_new(Path::new(&current_artifact.filename), b"current-managed")
        .expect("create current artifact");
    publish_managed_artifact_addition(storage.directory(), &current_artifact, &addition)
        .expect("publish current artifact");
    save_state(storage.directory(), &current_state).expect("save current state");
    reconcile_managed_addition_obligations(storage.directory(), Some(&current_state))
        .expect("settle current addition");
    settle_managed_artifact_removal(storage.directory(), &snapshot_artifact)
        .expect("settle snapshot removal");

    restore_rollback_snapshot_classified(storage.directory(), &snapshot)
        .expect("restore case-only snapshot identity");

    assert_eq!(
        load_state(storage.directory()).expect("load restored state"),
        Some(snapshot_state)
    );
    assert_eq!(
        fs::read(root.join("managed.jar")).expect("read restored artifact"),
        b"snapshot-managed"
    );
    let jar_names = fs::read_dir(&root)
        .expect("read managed root")
        .map(|entry| entry.expect("read managed entry"))
        .filter(|entry| entry.path().extension().is_some_and(|value| value == "jar"))
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert_eq!(jar_names, vec![std::ffi::OsString::from("managed.jar")]);
}

#[test]
fn rollback_failure_before_state_publication_preserves_old_authority() {
    let fixture = restore_compensation_fixture("rollback-before-state-publication");

    let result = restore_with_fault(
        fixture.storage.directory(),
        &fixture.snapshot,
        RollbackRestoreFaultPoint::BeforeStatePublication,
    );

    assert!(matches!(
        result,
        Err(RollbackRestoreError::Indeterminate(StateError::InvalidState(message)))
            if message == "injected rollback restore failure"
    ));
    assert_compensated_restore(
        &fixture,
        &fixture.current_state,
        "current.jar",
        b"current-managed",
        "target.jar",
    );
}

#[test]
fn rollback_failure_after_state_publication_preserves_target_authority() {
    let fixture = restore_compensation_fixture("rollback-after-state-publication");

    let result = restore_with_fault(
        fixture.storage.directory(),
        &fixture.snapshot,
        RollbackRestoreFaultPoint::AfterStatePublication,
    );

    assert!(matches!(
        result,
        Err(RollbackRestoreError::Indeterminate(StateError::InvalidState(message)))
            if message == "injected rollback restore failure"
    ));
    assert_compensated_restore(
        &fixture,
        &fixture.target_state,
        "target.jar",
        b"target-managed",
        "current.jar",
    );
}

#[test]
fn rollback_recovery_completes_a_metadata_first_partial_candidate() {
    let root = test_root("rollback-partial-candidate");
    let (storage, snapshot, candidate) = prepare_partial_candidate(&root);

    reconcile_rollback_metadata(storage.directory()).expect("resume candidate");

    assert!(!candidate.exists());
    assert_eq!(
        load_rollback_snapshot_by_id_admitted(storage.directory(), &snapshot.id)
            .expect("load recovered snapshot"),
        Some(snapshot)
    );
}

#[test]
fn unresumable_partial_candidate_is_durably_discarded() {
    let root = test_root("rollback-unresumable-candidate");
    let (storage, snapshot, candidate) = prepare_partial_candidate(&root);
    let tmp = rollback_tmp_path(&root);
    fs::remove_file(root.join("second.jar")).expect("remove recovery source");

    assert!(matches!(
        reconcile_rollback_metadata(storage.directory()),
        Err(StateError::RollbackCandidateUnresumable)
    ));
    assert!(!candidate.exists());
    assert!(
        !tmp.join(format!("{ROLLBACK_CANDIDATE_DELETE_PREFIX}{}", snapshot.id))
            .exists()
    );
    reconcile_rollback_metadata(storage.directory())
        .expect("discarded candidate must not wedge recovery");
}

#[test]
fn changed_source_discards_an_unresumable_partial_candidate() {
    let root = test_root("rollback-changed-source");
    let (storage, snapshot, candidate) = prepare_partial_candidate(&root);
    let tmp = rollback_tmp_path(&root);
    fs::write(root.join("second.jar"), b"changed-managed-second").expect("replace recovery source");

    assert!(matches!(
        reconcile_rollback_metadata(storage.directory()),
        Err(StateError::RollbackCandidateUnresumable)
    ));
    assert!(!candidate.exists());
    assert!(
        !tmp.join(format!("{ROLLBACK_CANDIDATE_DELETE_PREFIX}{}", snapshot.id))
            .exists()
    );
    assert_eq!(
        fs::read(root.join("second.jar")).expect("read changed source"),
        b"changed-managed-second"
    );
    reconcile_rollback_metadata(storage.directory())
        .expect("discarded changed-source candidate must not wedge recovery");
}

#[test]
fn unknown_candidate_entry_fails_closed_and_is_preserved() {
    let root = test_root("rollback-unknown-candidate-entry");
    let (storage, _, candidate) = prepare_partial_candidate(&root);
    let unknown = candidate.join("unknown.bin");
    fs::write(&unknown, b"unowned").expect("write unknown candidate entry");

    assert!(matches!(
        reconcile_rollback_metadata(storage.directory()),
        Err(StateError::InvalidRollback(reason))
            if reason == "rollback snapshot contains an unexpected entry"
    ));
    assert!(candidate.exists());
    assert_eq!(
        fs::read(unknown).expect("read preserved unknown entry"),
        b"unowned"
    );
}

#[test]
fn rollback_deletion_receipt_resumes_with_metadata_last() {
    let root = test_root("rollback-delete-receipt");
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("first.jar"), b"managed-first").expect("write first artifact");
    fs::write(root.join("second.jar"), b"managed-second").expect("write second artifact");
    let storage = TestManagedStorage::new(&root);
    let state = test_state(vec![
        test_installed_identity("first.jar", b"managed-first", "AANobbMI", "NFkjnzWE"),
        test_installed_identity("second.jar", b"managed-second", "BBNobbMI", "OFkjnzWE"),
    ]);
    let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");
    let history = rollback_history_path(&root);
    let delete_name = format!("{ROLLBACK_DELETE_PREFIX}{}", snapshot.id);
    let receipt = history.join(&delete_name);
    fs::rename(history.join(&snapshot.id), &receipt).expect("publish deletion receipt");
    fs::remove_file(receipt.join(&snapshot.artifacts[0].stored_filename))
        .expect("interrupt artifact deletion");

    reconcile_rollback_metadata(storage.directory()).expect("resume deletion receipt");

    assert!(!receipt.exists());
    assert!(
        list_rollback_snapshots_admitted(storage.directory())
            .expect("list history")
            .is_empty()
    );
}

#[test]
fn rollback_storage_budget_counts_actual_metadata_bytes() {
    let root = test_root("rollback-metadata-budget");
    fs::create_dir_all(&root).expect("create root");
    let storage = TestManagedStorage::new(&root);
    let snapshot = save_absent_rollback_snapshot(storage.directory()).expect("snapshot");
    let metadata_path = rollback_history_path(&root)
        .join(&snapshot.id)
        .join(ROLLBACK_METADATA_FILE_NAME);
    let mut metadata = fs::read(&metadata_path).expect("read metadata");
    metadata.extend(std::iter::repeat_n(b' ', 4096));
    fs::write(&metadata_path, &metadata).expect("expand metadata whitespace");
    let directory = rollback_snapshot_directory(storage.directory(), &snapshot.id)
        .expect("open snapshot directory");
    let admitted = read_snapshot_directory(&directory, &snapshot.id).expect("read snapshot");

    assert_eq!(
        rollback_directory_storage_bytes(&directory, &admitted).expect("measure snapshot storage"),
        metadata.len() as u64
    );
}

#[test]
fn persisted_candidate_rejects_aggregate_budget_before_source_copy() {
    let root = test_root("rollback-persisted-aggregate-budget");
    let candidate_id = "aggregate-budget";
    let candidate =
        rollback_tmp_path(&root).join(format!("{ROLLBACK_CANDIDATE_PREFIX}{candidate_id}"));
    fs::create_dir_all(&candidate).expect("create rollback candidate");
    let snapshot = RollbackSnapshot {
        id: candidate_id.to_string(),
        schema_version: ROLLBACK_SCHEMA_VERSION,
        created_at: Utc::now().to_rfc3339(),
        target: RollbackSnapshotState::ManagedStateAbsent,
        artifacts: vec![
            test_rollback_artifact("first.jar", MANAGED_ARTIFACT_MAX_BYTES),
            test_rollback_artifact("second.jar", 1),
        ],
    };
    fs::write(
        candidate.join(ROLLBACK_METADATA_FILE_NAME),
        serde_json::to_vec_pretty(&snapshot).expect("serialize oversized snapshot"),
    )
    .expect("write oversized snapshot intent");
    let storage = TestManagedStorage::new(&root);

    assert!(matches!(
        reconcile_rollback_metadata(storage.directory()),
        Err(StateError::InvalidRollback(reason))
            if reason == "rollback snapshot exceeds the aggregate artifact budget"
    ));
    assert_eq!(
        fs::read_dir(&candidate)
            .expect("read rejected candidate")
            .map(|entry| entry.expect("read candidate entry").file_name())
            .collect::<Vec<_>>(),
        vec![std::ffi::OsString::from(ROLLBACK_METADATA_FILE_NAME)]
    );
}

fn test_installed(filename: &str, bytes: &[u8]) -> InstalledMod {
    test_installed_identity(filename, bytes, "AANobbMI", "NFkjnzWE")
}

fn test_installed_identity(
    filename: &str,
    bytes: &[u8],
    project_id: &str,
    version_id: &str,
) -> InstalledMod {
    InstalledMod {
        project_id: project_id.to_string(),
        version_id: version_id.to_string(),
        filename: filename.to_string(),
        role: ManagedArtifactRole::Root,
        size: bytes.len() as u64,
        ownership_class: OwnershipClass::CompositionManaged,
        source: ManagedArtifactSource {
            provider: ManagedArtifactProvider::Modrinth,
        },
        integrity: ManagedArtifactIntegrity {
            sha512: hex::encode(Sha512::digest(bytes)),
        },
    }
}

fn test_rollback_artifact(filename: &str, size: u64) -> RollbackArtifact {
    RollbackArtifact {
        filename: filename.to_string(),
        stored_filename: format!("{filename}.bin"),
        project_id: "AANobbMI".to_string(),
        version_id: "NFkjnzWE".to_string(),
        ownership_class: OwnershipClass::CompositionManaged,
        size,
        sha512: "0".repeat(128),
    }
}

struct RestoreCompensationFixture {
    root: PathBuf,
    storage: TestManagedStorage,
    snapshot: RollbackSnapshot,
    target_state: CompositionState,
    current_state: CompositionState,
}

fn restore_compensation_fixture(name: &str) -> RestoreCompensationFixture {
    let root = test_root(name);
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("target.jar"), b"target-managed").expect("write target artifact");
    let storage = TestManagedStorage::new(&root);
    let target = test_installed_identity("target.jar", b"target-managed", "AANobbMI", "NFkjnzWE");
    let target_state = test_state(vec![target.clone()]);
    save_state(storage.directory(), &target_state).expect("save target state");
    let snapshot =
        save_rollback_snapshot(storage.directory(), &target_state).expect("save target snapshot");

    let current =
        test_installed_identity("current.jar", b"current-managed", "BBNobbMI", "OFkjnzWE");
    let current_state = test_state(vec![current.clone()]);
    stage_managed_artifact_removal(storage.directory(), &target).expect("stage target removal");
    let addition = prepare_managed_artifact_addition(storage.directory(), &current)
        .expect("prepare current addition");
    storage
        .directory()
        .create_file_create_new(Path::new(&current.filename), b"current-managed")
        .expect("create current artifact");
    publish_managed_artifact_addition(storage.directory(), &current, &addition)
        .expect("publish current addition");
    save_state(storage.directory(), &current_state).expect("save current state");
    reconcile_managed_addition_obligations(storage.directory(), Some(&current_state))
        .expect("settle current addition");
    settle_managed_artifact_removal(storage.directory(), &target).expect("settle target removal");

    RestoreCompensationFixture {
        root,
        storage,
        snapshot,
        target_state,
        current_state,
    }
}

fn restore_with_fault(
    instance_mods: &ManagedStorageDirectory,
    snapshot: &RollbackSnapshot,
    point: RollbackRestoreFaultPoint,
) -> Result<ManagedRollbackOutcome, RollbackRestoreError> {
    ROLLBACK_RESTORE_FAULT.with(|fault| {
        assert_eq!(
            fault.replace(Some(point)),
            None,
            "restore fault already armed"
        );
    });
    let result = restore_rollback_snapshot_classified(instance_mods, snapshot);
    ROLLBACK_RESTORE_FAULT.with(|fault| fault.set(None));
    result
}

fn assert_compensated_restore(
    fixture: &RestoreCompensationFixture,
    expected_state: &CompositionState,
    expected_filename: &str,
    expected_bytes: &[u8],
    absent_filename: &str,
) {
    let instance_mods = fixture.storage.directory();
    let admitted = load_state_admitted(instance_mods).expect("load compensated state");
    assert_eq!(admitted.as_ref(), Some(expected_state));
    assert_eq!(
        fs::read(fixture.root.join(expected_filename)).expect("read authoritative artifact"),
        expected_bytes
    );
    assert!(!fixture.root.join(absent_filename).exists());
    assert!(!managed_effect_reconciliation_required(instance_mods));
    assert_eq!(
        preflight_managed_inspection_reconciliation(instance_mods)
            .expect("preflight compensated storage"),
        ManagedInspectionReconciliation::default()
    );
    prove_managed_storage_recovered(instance_mods, admitted.as_ref())
        .expect("prove compensated storage");
    assert_no_pending_park_receipts(&fixture.root);
}

fn assert_no_pending_park_receipts(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).expect("read managed test directory") {
            let entry = entry.expect("read managed test entry");
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(EMPTY_DIRECTORY_PARK_PREFIX)
                    && !name.starts_with(ROLLBACK_DELETE_PREFIX),
                "pending managed park or deletion receipt remained: {name}"
            );
            if entry
                .file_type()
                .expect("read managed test entry type")
                .is_dir()
            {
                pending.push(entry.path());
            }
        }
    }
}

fn prepare_partial_candidate(root: &Path) -> (TestManagedStorage, RollbackSnapshot, PathBuf) {
    fs::create_dir_all(root).expect("create root");
    fs::write(root.join("first.jar"), b"managed-first").expect("write first artifact");
    fs::write(root.join("second.jar"), b"managed-second").expect("write second artifact");
    let storage = TestManagedStorage::new(root);
    let state = test_state(vec![
        test_installed_identity("first.jar", b"managed-first", "AANobbMI", "NFkjnzWE"),
        test_installed_identity("second.jar", b"managed-second", "BBNobbMI", "OFkjnzWE"),
    ]);
    let snapshot = save_rollback_snapshot(storage.directory(), &state).expect("snapshot");
    let candidate =
        rollback_tmp_path(root).join(format!("{ROLLBACK_CANDIDATE_PREFIX}{}", snapshot.id));
    fs::rename(rollback_history_path(root).join(&snapshot.id), &candidate)
        .expect("restore candidate namespace");
    let missing = snapshot
        .artifacts
        .iter()
        .find(|artifact| artifact.filename == "second.jar")
        .expect("second snapshot artifact");
    fs::remove_file(candidate.join(&missing.stored_filename)).expect("interrupt candidate copy");
    (storage, snapshot, candidate)
}

fn test_state(installed_mods: Vec<InstalledMod>) -> CompositionState {
    let mut state = CompositionState {
        composition_id: "test-composition".to_string(),
        family: VersionFamily::A,
        tier: CompositionTier::Core,
        game_version: "1.21.1".to_string(),
        loader: "fabric".to_string(),
        graph_sha512: String::new(),
        dependency_edges: Vec::<ManagedDependencyStateEdge>::new(),
        installed_mods,
        installed_at: Utc::now().to_rfc3339(),
    };
    state.graph_sha512 =
        crate::performance::composition::plan::canonical_state_graph_digest(&state)
            .expect("canonical test state graph");
    state
}

fn test_root(name: &str) -> PathBuf {
    std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary parent")
        .join(format!(
            "axial-performance-state-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
        .join("managed")
}

fn rollback_history_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR_NAME)
        .join(ROLLBACK_DIR_NAME)
        .join(ROLLBACK_HISTORY_DIR_NAME)
}

fn rollback_tmp_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR_NAME)
        .join(ROLLBACK_DIR_NAME)
        .join(ROLLBACK_TMP_DIR_NAME)
}
