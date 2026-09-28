use super::*;
use crate::library::{LibraryLifecycle, LibraryOpenOutcome};

fn temporary() -> tempfile::TempDir {
    tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
}

fn fixture(path: &std::path::Path) -> LibraryLifecycle {
    match LibraryLifecycle::open(path) {
        LibraryOpenOutcome::Ready(owner) => owner,
        outcome => panic!("could not open fixture: {outcome:?}"),
    }
}

fn finish(owner: LibraryLifecycle) {
    owner.close_admission();
    assert!(matches!(
        owner.revoke_application_root().unwrap(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[test]
fn scoped_projection_and_receipt_follow_exact_directory_identity() {
    let temp = temporary();
    std::fs::create_dir(temp.path().join("instance")).unwrap();
    let owner = fixture(temp.path());
    let root = owner.admit().unwrap().files().unwrap();
    let child = root
        .open_directory(&PortableName::new_exact("instance").unwrap())
        .unwrap();
    let receipt = child.receipt().unwrap();
    assert!(child.matches_receipt(&receipt).unwrap());
    assert_eq!(
        child.read_projection().unwrap(),
        temp.path().join("instance")
    );
    std::fs::rename(temp.path().join("instance"), temp.path().join("original")).unwrap();
    std::fs::create_dir(temp.path().join("instance")).unwrap();
    assert!(child.read_projection().is_err());
    let replacement = root
        .open_directory(&PortableName::new_exact("instance").unwrap())
        .unwrap();
    assert!(!replacement.matches_receipt(&receipt).unwrap());
    drop((replacement, child, root));
    finish(owner);
}

#[test]
fn directory_park_receipt_roundtrip_and_malformed_unicode_are_bounded() {
    let temp = temporary();
    std::fs::create_dir(temp.path().join("instance")).unwrap();
    let owner = fixture(temp.path());
    let root = owner.admit().unwrap().files().unwrap();
    let plan = root
        .plan_park(
            &PortableName::new_exact("instance").unwrap(),
            &PortableName::new_exact("parked").unwrap(),
        )
        .unwrap();
    let receipt = DirectoryParkReceipt::decode(&plan.receipt().encode()).unwrap();
    assert!(matches!(
        root.recover_park(&receipt).unwrap(),
        RecoveredDirectoryPark::Original(_)
    ));
    let malformed = format!("axial-dir-v1:{}é:{}", "0".repeat(63), "0".repeat(63));
    assert_eq!(malformed.len(), 142);
    let raw = serde_json::json!({"schema":1,"parent":malformed,"source":malformed,
        "source_name":"instance","park_name":"parked"})
    .to_string();
    assert!(DirectoryParkReceipt::decode(&raw).is_err());
    drop((plan, root));
    finish(owner);
}

#[test]
fn empty_stage_payload_still_claims_its_only_writer() {
    let temp = temporary();
    let owner = fixture(temp.path());
    let root = owner.admit().unwrap().files().unwrap();
    let mut stage = match root.stage(&PortableName::new_exact("payload").unwrap()) {
        StageCreateOutcome::Created(stage) => stage,
        outcome => panic!("unexpected create: {outcome:?}"),
    };
    stage.write_all(b"").unwrap();
    assert!(stage.write_all(b"replacement").is_err());
    let (outcome, pin) = stage.discard().into_parts();
    assert!(matches!(outcome, axial_fs::StageDiscardOutcome::Discarded));
    drop((pin, root));
    finish(owner);
}

#[test]
fn unrelated_directory_cannot_be_paired_with_a_library_pin() {
    let first = temporary();
    let second = temporary();
    let first_owner = fixture(first.path());
    let second_owner = fixture(second.path());
    let first_pin = first_owner.admit().unwrap();
    let second_pin = second_owner.admit().unwrap();
    let directory = first_pin.directory().unwrap();
    assert!(ScopedDirectory::from_admitted(directory, second_pin).is_err());
    drop(first_pin);
    finish(first_owner);
    finish(second_owner);
}
