use super::{PROFILE_MARKER, StartupError, admit_profile, start_in_profile};
use axial_fs::{LeafName, PendingRootReset, RootSession, RootSessionAcquireOutcome};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const CHILD_PROFILE: &str = "AXIAL_TEST_INTERRUPTED_RESET_PROFILE";
const CHILD_EXIT: i32 = 73;
const RESET_INTENT: &str = ".axial-reset-intent";

fn profile_fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let temporary = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let root = temporary.path().join("rewrite");
    admit_profile(&root).unwrap();
    let marker = fs::read(root.join(PROFILE_MARKER)).unwrap();
    fs::write(root.join("keep.txt"), b"profile user data").unwrap();
    fs::create_dir(root.join("worlds")).unwrap();
    fs::write(root.join("worlds/world.dat"), b"nested user data").unwrap();
    (temporary, root, marker)
}

fn assert_profile_preserved(root: &Path, marker: &[u8]) {
    assert_eq!(fs::read(root.join(PROFILE_MARKER)).unwrap(), marker);
    assert_eq!(
        fs::read(root.join("keep.txt")).unwrap(),
        b"profile user data"
    );
    assert_eq!(
        fs::read(root.join("worlds/world.dat")).unwrap(),
        b"nested user data"
    );
    assert!(!root.join("metadata.sqlite").exists());
}

fn interrupt_accepted_reset(root: &Path) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "reset_tests::accepted_reset_crash_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_PROFILE, root)
        .output()
        .expect("run accepted reset subprocess");
    let diagnostics = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tail = diagnostics.lines().rev().take(30).collect::<Vec<_>>();
    assert_eq!(output.status.code(), Some(CHILD_EXIT), "{tail:?}");
}

async fn startup_failure(root: &Path) -> StartupError {
    match start_in_profile(root.to_path_buf(), None).await {
        Err(failure) => failure,
        Ok(services) => {
            services.server.shutdown().await.unwrap();
            panic!("interrupted reset must block service startup");
        }
    }
}

#[test]
#[ignore = "subprocess helper that exits with an accepted reset still pending"]
fn accepted_reset_crash_helper() {
    let root = PathBuf::from(std::env::var_os(CHILD_PROFILE).expect("isolated child profile"));
    let session = match RootSession::acquire(&root) {
        RootSessionAcquireOutcome::Acquired(session) => session,
        outcome => panic!("child root admission failed: {outcome:?}"),
    };
    let mut pending = PendingRootReset::new(
        session,
        LeafName::new(PROFILE_MARKER).unwrap(),
        fs::read(root.join(PROFILE_MARKER)).unwrap(),
    );
    pending.prepare().expect("persist accepted reset intent");
    std::process::exit(CHILD_EXIT);
}

#[tokio::test]
async fn interrupted_reset_preserves_profile_until_fresh_confirmation_then_restarts() {
    let (temporary, root, marker) = profile_fixture();
    let outside = temporary.path().join("outside.txt");
    fs::write(&outside, b"outside user data").unwrap();
    interrupt_accepted_reset(&root);
    assert_profile_preserved(&root, &marker);

    let failure = startup_failure(&root).await;
    assert!(failure.interrupted_reset());
    assert_profile_preserved(&root, &marker);
    failure
        .try_preserve()
        .expect("preserve interrupted profile and release lease");
    assert_profile_preserved(&root, &marker);

    let mut failure = startup_failure(&root).await;
    assert!(failure.interrupted_reset());
    let session = failure
        .take_interrupted_reset_session()
        .expect("fresh confirmation transfers the exact admitted root");
    assert!(!failure.interrupted_reset());
    assert!(failure.take_interrupted_reset_session().is_err());
    failure
        .try_preserve()
        .expect("startup failure no longer owns the root");
    assert_profile_preserved(&root, &marker);

    let mut pending = PendingRootReset::new(
        session,
        LeafName::new(PROFILE_MARKER).unwrap(),
        marker.clone(),
    );
    pending.try_clear().expect("finish newly confirmed reset");
    pending.try_clear().expect("completed reset is idempotent");
    assert_eq!(fs::read(root.join(PROFILE_MARKER)).unwrap(), marker);
    assert!(!root.join("keep.txt").exists());
    assert!(!root.join("worlds").exists());
    assert!(!root.join("metadata.sqlite").exists());
    assert_eq!(fs::read(outside).unwrap(), b"outside user data");

    let restarted = start_in_profile(root, None).await.unwrap();
    assert_eq!(restarted.settings.current().unwrap().revision, 0);
    restarted.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_accepted_reset_intent_blocks_startup_without_offering_deletion() {
    let (_temporary, root, marker) = profile_fixture();
    interrupt_accepted_reset(&root);
    let intent_path = root.join(RESET_INTENT);
    let mut malformed = fs::read(&intent_path).unwrap();
    assert!(!malformed.is_empty());
    malformed.truncate(malformed.len() / 2);
    fs::write(&intent_path, &malformed).unwrap();

    let mut failure = startup_failure(&root).await;
    assert!(!failure.interrupted_reset());
    assert!(failure.take_interrupted_reset_session().is_err());
    failure
        .try_preserve()
        .expect("release without deleting malformed evidence");
    assert_profile_preserved(&root, &marker);
    assert_eq!(fs::read(intent_path).unwrap(), malformed);
}

#[tokio::test]
async fn old_reset_record_preserves_profile_without_consulting_metadata() {
    for metadata_present in [false, true] {
        let (_temporary, root, marker) = profile_fixture();
        interrupt_accepted_reset(&root);
        let intent_path = root.join(RESET_INTENT);
        let mut old = fs::read(&intent_path).unwrap();
        let current_prefix = b"axial-root-reset-v2\0";
        assert!(old.starts_with(current_prefix));
        old[..current_prefix.len()].copy_from_slice(b"axial-root-reset-v1\0");
        fs::write(&intent_path, &old).unwrap();
        let metadata = root.join("metadata.sqlite");
        if metadata_present {
            fs::write(&metadata, b"partially removed metadata").unwrap();
        }

        let mut failure = startup_failure(&root).await;
        assert!(!failure.interrupted_reset());
        assert!(failure.take_interrupted_reset_session().is_err());
        failure
            .try_preserve()
            .expect("preserve ambiguous old reset");
        assert_eq!(fs::read(root.join(PROFILE_MARKER)).unwrap(), marker);
        assert_eq!(
            fs::read(root.join("keep.txt")).unwrap(),
            b"profile user data"
        );
        assert_eq!(
            fs::read(root.join("worlds/world.dat")).unwrap(),
            b"nested user data"
        );
        assert_eq!(fs::read(intent_path).unwrap(), old);
        if metadata_present {
            assert_eq!(fs::read(metadata).unwrap(), b"partially removed metadata");
        } else {
            assert!(!metadata.exists());
        }
    }
}

#[tokio::test]
async fn unknown_reset_intent_blocks_startup_without_offering_deletion() {
    let (_temporary, root, marker) = profile_fixture();
    let intent_path = root.join(RESET_INTENT);
    let unknown = b"unknown user-owned reset record";
    fs::write(&intent_path, unknown).unwrap();

    let mut failure = startup_failure(&root).await;
    assert!(!failure.interrupted_reset());
    assert!(failure.take_interrupted_reset_session().is_err());
    failure
        .try_preserve()
        .expect("release without adopting unknown evidence");
    assert_profile_preserved(&root, &marker);
    assert_eq!(fs::read(intent_path).unwrap(), unknown);
}

#[tokio::test]
async fn unknown_launch_binding_blocks_before_runtime_recovery_and_can_be_preserved() {
    use axial_app::{
        launch::coordinator::INTENT_MIGRATION,
        storage::{MetadataStore, StorageError, rusqlite::params},
    };

    for malformed in [false, true] {
        let (_temporary, root, marker) = profile_fixture();
        let key = uuid::Uuid::new_v4().to_string();
        let payload = if malformed {
            b"invalid launch record".to_vec()
        } else {
            serde_json::to_vec(&serde_json::json!({
                "request": {"instance_id": uuid::Uuid::new_v4().to_string(), "intent_key": key},
                "context": null,
                "session_id": uuid::Uuid::new_v4().to_string()
            }))
            .unwrap()
        };
        let storage = MetadataStore::open(root.join("metadata.sqlite")).unwrap();
        storage.migrate(&[INTENT_MIGRATION]).unwrap();
        storage
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute(
                    "INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'accepted')",
                    params![key, payload],
                )?;
                Ok(())
            })
            .unwrap();
        drop(storage);

        let mut failure = startup_failure(&root).await;
        assert!(!failure.interrupted_reset());
        assert!(failure.take_interrupted_reset_session().is_err());
        assert!(!root.join("runtime").exists());
        failure
            .try_preserve()
            .expect("unknown launch does not trap startup preservation");
        assert_eq!(fs::read(root.join(PROFILE_MARKER)).unwrap(), marker);
        assert_eq!(
            fs::read(root.join("keep.txt")).unwrap(),
            b"profile user data"
        );
        assert_eq!(
            fs::read(root.join("worlds/world.dat")).unwrap(),
            b"nested user data"
        );
        let storage = MetadataStore::open(root.join("metadata.sqlite")).unwrap();
        let persisted: Vec<u8> = storage
            .read(|db| -> Result<_, StorageError> {
                Ok(db.query_row(
                    "SELECT payload FROM launch_intents WHERE intent_key=?1",
                    [&key],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(persisted, payload);
        drop(storage);
        startup_failure(&root)
            .await
            .try_preserve()
            .expect("repeat startup remains preservable");
    }
}
