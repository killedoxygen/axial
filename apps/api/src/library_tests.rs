use super::*;
use axial_app::{
    instances::{
        create::{CreateInstanceRequest, CreateTarget},
        delete::{DeleteIntent, DeletionError, DeletionStatus},
        model::{InstanceError, InstanceLifecycle},
    },
    library::{AdmissionState, LibraryMode},
    storage::{
        StorageError,
        rusqlite::{self, types::Value as SqlValue},
    },
};
use serde_json::{Value, json};

fn temporary() -> tempfile::TempDir {
    tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
}

fn select(root: &Path, external: &Path, library_id: LibraryId) {
    fs::write(
        root.join("library.json"),
        serde_json::to_vec(&json!({
            "mode":"existing", "library_id":library_id.to_string(), "path":external,
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn get(services: &DesktopServices, route: &str) -> reqwest::Response {
    let bootstrap = services.server.bootstrap();
    reqwest::Client::new()
        .get(format!("{}/api/v1/{route}", bootstrap.base_url))
        .header(transport::CAPABILITY_HEADER, bootstrap.capability)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn external_selection_precedes_instance_composition_and_survives_reopen() {
    let temporary = temporary();
    let root = temporary.path().join("replacement");
    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("keep.txt"), b"unrelated payload").unwrap();
    let initial = start_in_profile(root.clone(), None).await.unwrap();
    initial.server.shutdown().await.unwrap();
    drop(initial);
    let library_id = LibraryId::new();
    select(&root, &external, library_id);
    let services = start_in_profile(root.clone(), None).await.unwrap();
    let pin = services.library.admit().unwrap();
    assert_eq!(pin.library_id(), library_id);
    assert_eq!(pin.read_projection().unwrap(), external);
    assert_eq!(
        services.library.snapshot().current.unwrap().mode,
        LibraryMode::Existing
    );
    drop(pin);
    let status: Value = get(&services, "status")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["setup_required"], false);
    let target = CreateTarget::loader_for_tests(
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.9",
    )
    .unwrap();
    let instance = services
        .instances
        .create(
            CreateInstanceRequest {
                name: "External instance".into(),
                selection_id: target.selection_id().to_owned(),
                ..Default::default()
            },
            target,
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        services
            .instances
            .registry()
            .get_live(&instance.id)
            .unwrap()
            .library_id,
        library_id.to_string()
    );
    assert!(!root.join("instances").exists());
    assert!(external.join("instances").is_dir());
    services.server.shutdown().await.unwrap();
    drop(services);
    let reopened = start_in_profile(root, None).await.unwrap();
    assert_eq!(reopened.library.admit().unwrap().library_id(), library_id);
    let instances: Value = get(&reopened, "instances")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(instances["instances"][0]["id"], instance.id.as_str());
    assert_eq!(
        fs::read(external.join("keep.txt")).unwrap(),
        b"unrelated payload"
    );
    reopened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_selected_library_never_creates_or_falls_back_to_managed() {
    let temporary = temporary();
    let root = temporary.path().join("replacement");
    let external = temporary.path().join("missing");
    let initial = start_in_profile(root.clone(), None).await.unwrap();
    initial.server.shutdown().await.unwrap();
    drop(initial);
    select(&root, &external, LibraryId::new());
    let services = start_in_profile(root.clone(), None).await.unwrap();
    assert_eq!(
        services.library.snapshot().admission,
        AdmissionState::Unavailable
    );
    assert!(services.library.snapshot().current.is_none());
    assert!(!external.exists());
    assert!(!root.join("instances").exists());
    let status: Value = get(&services, "status")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["setup_required"], false);
    assert!(
        status["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("external library is unavailable"))
    );
    for route in ["versions", "instances"] {
        assert_eq!(
            get(&services, route).await.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let bootstrap = services.server.bootstrap();
    assert_eq!(
        reqwest::Client::new()
            .post(format!("{}/api/v1/setup/init", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, bootstrap.capability)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(services.library.admit_application_root().is_ok());
    services.server.shutdown().await.unwrap();
    assert!(!external.exists());
}

#[tokio::test]
async fn invalid_library_selection_refuses_startup_and_preserves_configuration() {
    let temporary = temporary();
    let root = temporary.path().join("replacement");
    let initial = start_in_profile(root.clone(), None).await.unwrap();
    initial.server.shutdown().await.unwrap();
    drop(initial);
    let bytes = br#"{"mode":"existing","library_id":"00000000-0000-0000-0000-000000000000","path":"relative"}"#;
    fs::write(root.join("library.json"), bytes).unwrap();
    let failure = match start_in_profile(root.clone(), None).await {
        Ok(services) => {
            services.server.shutdown().await.unwrap();
            panic!("invalid selection started");
        }
        Err(failure) => failure,
    };
    assert!(failure.try_preserve().is_ok());
    assert_eq!(fs::read(root.join("library.json")).unwrap(), bytes);
}

const PENDING_INSTANCE_PROFILE: &str = "AXIAL_TEST_PENDING_INSTANCE_PROFILE";
const PENDING_INSTANCE_DELETE: &str = "AXIAL_TEST_PENDING_INSTANCE_DELETE";
const PENDING_INSTANCE_EXIT: i32 = 79;

fn instance_intent_evidence(storage: &MetadataStore) -> [Vec<Vec<SqlValue>>; 3] {
    [
        "SELECT * FROM instances ORDER BY rowid",
        "SELECT * FROM instance_creations ORDER BY rowid",
        "SELECT * FROM instance_deletions ORDER BY rowid",
    ]
    .map(|query| {
        storage
            .read(|db| -> Result<_, StorageError> {
                let mut statement = db.prepare(query)?;
                let columns = statement.column_count();
                Ok(statement
                    .query_map([], |row| {
                        (0..columns).map(|column| row.get(column)).collect()
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .unwrap()
    })
}

#[tokio::test]
async fn interrupted_external_creation_blocks_reset_but_allows_preserved_exit() {
    disconnected_instance_intent(false).await;
}

#[tokio::test]
async fn interrupted_external_deletion_blocks_reset_but_allows_preserved_exit() {
    disconnected_instance_intent(true).await;
}

async fn disconnected_instance_intent(deletion: bool) {
    use std::io::{Read, Seek, SeekFrom};

    let temporary = temporary();
    let profile = admit_profile(&temporary.path().join("replacement")).unwrap();
    let root = profile.root;
    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    assert_eq!(fs::canonicalize(&external).unwrap(), external);
    fs::write(external.join("keep.txt"), b"unrelated payload").unwrap();
    let library_id = LibraryId::new();
    select(&root, &external, library_id);
    let selection = fs::read(root.join("library.json")).unwrap();
    let profile_identity = fs::read(root.join(PROFILE_MARKER)).unwrap();
    let mut output = tempfile::tempfile().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "library_tests::pending_external_instance_exit_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(PENDING_INSTANCE_PROFILE, &root)
        .env(PENDING_INSTANCE_DELETE, if deletion { "1" } else { "0" })
        .stdout(output.try_clone().unwrap())
        .stderr(output.try_clone().unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    if result.is_err() {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
    output
        .seek(SeekFrom::Start(
            output.metadata().unwrap().len().saturating_sub(4096),
        ))
        .unwrap();
    let mut tail = String::new();
    output.read_to_string(&mut tail).unwrap();
    assert_eq!(
        result.expect(&tail).unwrap().code(),
        Some(PENDING_INSTANCE_EXIT),
        "{tail}"
    );

    let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
    let evidence = instance_intent_evidence(&storage);
    storage
        .transaction(|db| -> Result<(), StorageError> {
            db.execute_batch("DROP TRIGGER refuse_instance_publication;")?;
            Ok(())
        })
        .unwrap();
    let registry = axial_app::instances::directory::Registry::new(storage.clone());
    let pending = registry.pending().unwrap();
    assert_eq!(pending.len(), 1);
    let record = &pending[0];
    assert_eq!(record.library_id, library_id.to_string());
    assert_eq!(
        record.lifecycle,
        if deletion {
            InstanceLifecycle::Deleting
        } else {
            InstanceLifecycle::Reserved
        }
    );
    let payload = external.join("instances").join(&record.directory_name);
    let observer = match axial_fs::RootSession::acquire(temporary.path()) {
        axial_fs::RootSessionAcquireOutcome::Acquired(observer) => observer,
        other => panic!("fixture identity observation failed: {other:?}"),
    };
    let identity = |path: &Path| {
        observer
            .admit_absolute_directory(path)
            .unwrap()
            .identity()
            .unwrap()
            .filesystem_identity()
    };
    let external_identity = identity(&external);
    let payload_identity = identity(&payload);
    assert_eq!(
        fs::read(payload.join("saves/keep.txt")).unwrap(),
        b"instance payload"
    );
    let disconnected = temporary.path().join("disconnected");
    fs::rename(&external, &disconnected).unwrap();
    let payload = disconnected.join("instances").join(&record.directory_name);
    drop(registry);
    drop(storage);

    let services = start_in_profile(root.clone(), None).await.unwrap();
    assert_eq!(services.profile_root, root);
    assert_eq!(
        services.library.snapshot().admission,
        AdmissionState::Unavailable
    );
    assert!(services.library.snapshot().current.is_none());
    assert_eq!(
        instance_intent_evidence(services.instances.registry().storage()),
        evidence
    );
    assert!(services.instances.has_pending_intents());
    assert!(!services.instances.has_unsettled_effects());
    let pending_deletions = services.instances.pending_deletions().unwrap();
    assert_eq!(pending_deletions.len(), usize::from(deletion));
    assert!(services.server.ensure_reset_allowed().is_err());
    assert!(!services.server.is_shutdown_settled());
    assert!(get(&services, "status").await.status().is_success());
    assert_eq!(
        get(&services, "instances").await.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    services.server.shutdown().await.unwrap();
    assert!(services.server.is_shutdown_settled());
    assert_eq!(
        instance_intent_evidence(services.instances.registry().storage()),
        evidence
    );
    assert_eq!(fs::read(root.join("library.json")).unwrap(), selection);
    assert_eq!(
        fs::read(root.join(PROFILE_MARKER)).unwrap(),
        profile_identity
    );
    assert_eq!(identity(&disconnected), external_identity);
    assert_eq!(identity(&payload), payload_identity);
    assert_eq!(
        fs::read(payload.join("saves/keep.txt")).unwrap(),
        b"instance payload"
    );
    assert_eq!(
        fs::read(disconnected.join("keep.txt")).unwrap(),
        b"unrelated payload"
    );
    assert!(!external.exists());
    assert!(!root.join("instances").exists());
    drop(services);

    fs::rename(&disconnected, &external).unwrap();
    let recovered = start_in_profile(root.clone(), None).await.unwrap();
    let pin = recovered.library.admit().unwrap();
    assert_eq!(pin.library_id(), library_id);
    assert_eq!(pin.read_projection().unwrap(), external);
    drop(pin);
    assert_eq!(
        recovered.library.snapshot().current.unwrap().mode,
        LibraryMode::Existing
    );
    assert!(!recovered.instances.has_pending_intents());
    assert!(!recovered.instances.has_unsettled_effects());
    let live = recovered
        .instances
        .registry()
        .get_live(&record.instance.id)
        .unwrap();
    assert_eq!(live.library_id, library_id.to_string());
    assert_eq!(live.directory_name, record.directory_name);
    if deletion {
        assert_eq!(
            recovered
                .instances
                .deletion_status(pending_deletions[0].operation_id)
                .unwrap()
                .status,
            DeletionStatus::Aborted
        );
    }
    assert!(recovered.server.ensure_reset_allowed().is_ok());
    let instances: Value = get(&recovered, "instances")
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(instances["instances"][0]["id"], record.instance.id.as_str());
    recovered.server.shutdown().await.unwrap();
    let payload = external.join("instances").join(&record.directory_name);
    assert_eq!(identity(&external), external_identity);
    assert_eq!(identity(&payload), payload_identity);
    assert_eq!(
        fs::read(payload.join("saves/keep.txt")).unwrap(),
        b"instance payload"
    );
    assert_eq!(
        fs::read(external.join("keep.txt")).unwrap(),
        b"unrelated payload"
    );
    assert_eq!(fs::read(root.join("library.json")).unwrap(), selection);
    assert_eq!(
        fs::read(root.join(PROFILE_MARKER)).unwrap(),
        profile_identity
    );
    assert!(!root.join("instances").exists());
    assert!(matches!(
        observer.revoke(),
        axial_fs::RootRevokeOutcome::Revoked
    ));
}

#[tokio::test]
#[ignore = "subprocess helper exiting after an injected instance publication refusal"]
async fn pending_external_instance_exit_helper() {
    let root = PathBuf::from(std::env::var_os(PENDING_INSTANCE_PROFILE).unwrap());
    assert_eq!(fs::canonicalize(&root).unwrap(), root);
    let deletion = std::env::var(PENDING_INSTANCE_DELETE).unwrap() == "1";
    let services = start_in_profile(root.clone(), None).await.unwrap();
    let selection: Value =
        serde_json::from_slice(&fs::read(root.join("library.json")).unwrap()).unwrap();
    let pin = services.library.admit().unwrap();
    let external = pin.read_projection().unwrap();
    assert_eq!(
        pin.library_id().to_string(),
        selection["library_id"].as_str().unwrap()
    );
    assert_eq!(external, PathBuf::from(selection["path"].as_str().unwrap()));
    drop(pin);
    let storage = services.instances.registry().storage();
    let refuse_publication = |sql: &str| {
        storage
            .transaction(|db| -> Result<(), StorageError> {
                db.execute_batch(sql)?;
                Ok(())
            })
            .unwrap();
    };
    if !deletion {
        refuse_publication(
            "CREATE TRIGGER refuse_instance_publication BEFORE UPDATE ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(ABORT, 'injected creation publication refusal'); END;",
        );
    }
    let target = CreateTarget::loader_for_tests(
        axial_minecraft::LoaderComponentId::Fabric,
        "1.21.1",
        "0.16.9",
    )
    .unwrap();
    let created = services
        .instances
        .create(
            CreateInstanceRequest {
                name: "Interrupted external instance".into(),
                selection_id: target.selection_id().to_owned(),
                ..Default::default()
            },
            target,
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    let id = if deletion {
        created.unwrap().id
    } else {
        assert!(
            matches!(created, Err(InstanceError::Storage(StorageError::Sqlite(rusqlite::Error::SqliteFailure(_, Some(message))))) if message == "injected creation publication refusal")
        );
        let pending = services.instances.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].phase, "published");
        pending[0].instance_id.clone()
    };
    let record = services.instances.registry().get_record(&id).unwrap();
    let (phase, receipt): (String, Option<String>) = storage
        .read(|db| -> Result<_, StorageError> {
            Ok(db.query_row(
                "SELECT phase,directory_receipt FROM instance_creations WHERE instance_id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(phase, if deletion { "complete" } else { "published" });
    assert!(receipt.is_some_and(|receipt| !receipt.is_empty()));
    let payload = external.join("instances").join(&record.directory_name);
    assert!(
        !external
            .join("instances")
            .join(format!("stage-{id}"))
            .exists()
    );
    assert!(payload.join("mods").is_dir());
    fs::write(payload.join("saves/keep.txt"), b"instance payload").unwrap();
    if deletion {
        refuse_publication(
            "CREATE TRIGGER refuse_instance_publication BEFORE UPDATE ON instance_deletions WHEN NEW.phase IN ('committed','aborted') BEGIN SELECT RAISE(ABORT, 'injected deletion publication refusal'); END;",
        );
        let operation = uuid::Uuid::new_v4();
        assert!(
            matches!(services.instances.delete(&id, DeleteIntent::DeleteFiles, operation).unwrap().join().await.unwrap(), Err(DeletionError::Storage(StorageError::Sqlite(rusqlite::Error::SqliteFailure(_, Some(message))))) if message == "injected deletion publication refusal")
        );
        let pending = services.instances.pending_deletions().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, operation);
        assert_eq!(pending[0].instance_id, id);
        assert_eq!(pending[0].intent, DeleteIntent::DeleteFiles);
        assert_eq!(pending[0].status, DeletionStatus::PendingRestore);
        assert!(
            !external
                .join("instances")
                .join(format!("deleted-{operation}"))
                .exists()
        );
    }
    assert_eq!(
        fs::read(payload.join("saves/keep.txt")).unwrap(),
        b"instance payload"
    );
    assert!(services.instances.has_pending_intents());
    assert!(services.instances.has_unsettled_effects());
    assert!(services.server.ensure_reset_allowed().is_err());
    // The injected database refusal retains the effect; this later exit loses its owner.
    std::process::exit(PENDING_INSTANCE_EXIT);
}
