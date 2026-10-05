use super::*;
use axial_app::{
    content::{
        install::MutationError,
        model::{CanonicalId, ProviderId},
        packs::{PackArchive, ResolvedPack},
        provenance::{ContentManifest, MANIFEST_FILE},
    },
    instances::{
        create::{CreateInstanceRequest, CreateTarget},
        delete::{DeleteIntent, DeletionError, DeletionStatus},
        model::{InstanceError, InstanceId, InstanceLifecycle},
    },
    library::{AdmissionState, LibraryMode},
    storage::{
        StorageError,
        rusqlite::{self, types::Value as SqlValue},
    },
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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

const PENDING_CONTENT_PROFILE: &str = "AXIAL_TEST_PENDING_CONTENT_PROFILE";
const PENDING_CONTENT_QUEUED: &str = "AXIAL_TEST_PENDING_CONTENT_QUEUED";
const PENDING_CONTENT_PUBLIC_MOVE: &str = "AXIAL_TEST_PENDING_CONTENT_PUBLIC_MOVE";
const PENDING_CONTENT_EXIT: i32 = 42;
const CONTENT_OVERRIDE: &[u8] = b"preserved content override\n";
const CONTENT_PUBLIC_PATH: &str = "config/published.txt";
const CONTENT_PUBLIC_BYTES: &[u8] = b"published before manifest commit\n";

fn override_pack() -> ResolvedPack {
    override_pack_at("config/preserved.txt", CONTENT_OVERRIDE)
}

fn override_pack_at(path: &str, bytes: &[u8]) -> ResolvedPack {
    let mut archive = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    for (path, bytes) in [
        (
            "modrinth.index.json".to_owned(),
            serde_json::to_vec(&json!({
                "name":"Interrupted content fixture",
                "dependencies":{"minecraft":"1.21.1", "fabric-loader":"0.16.9"},
                "files":[],
            }))
            .unwrap(),
        ),
        (format!("overrides/{path}"), bytes.to_vec()),
    ] {
        archive
            .start_file(path, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&bytes).unwrap();
    }
    ResolvedPack {
        canonical_id: CanonicalId::for_project(ProviderId::Modrinth, "fixture-pack"),
        version_id: "fixture-v1".into(),
        name: "Interrupted content fixture".into(),
        archive: PackArchive::read(archive.finish().unwrap().into_inner()).unwrap(),
    }
}

fn content_receipt(storage: &MetadataStore, id: &InstanceId) -> (String, String) {
    storage
        .read(|db| -> Result<_, StorageError> {
            Ok(db.query_row(
                "SELECT operation_id,receipt_json FROM content_batches WHERE instance_id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap()
}

fn content_queue_rows(storage: &MetadataStore, status: bool) -> Vec<Vec<SqlValue>> {
    let query = if status {
        "SELECT * FROM install_queue ORDER BY rowid"
    } else {
        "SELECT id,operation_id,library_id,request_json,target_json,accepted_at,checkpoint_json FROM install_queue ORDER BY rowid"
    };
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
}

fn content_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut tree = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            let bytes = if kind.is_dir() {
                directories.push(path.clone());
                None
            } else {
                assert!(kind.is_file());
                assert!(entry.metadata().unwrap().len() <= 64 * 1024);
                Some(fs::read(&path).unwrap())
            };
            tree.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
            assert!(tree.len() <= 64);
        }
    }
    tree
}

async fn content_fence(services: &DesktopServices, id: &InstanceId) -> (u16, bool, bool, bool) {
    let read = get(services, &format!("instances/{id}/content"))
        .await
        .status()
        .as_u16();
    let service =
        ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap()).unwrap();
    let refused = match services
        .content_mutations
        .install_pack(&service, id, override_pack(), true)
    {
        Err(MutationError::Unavailable) => true,
        Err(_) => false,
        Ok(task) => {
            let _ = task.join().await.unwrap();
            false
        }
    };
    (
        read,
        refused,
        services.server.ensure_reset_allowed().is_err(),
        services.server.ensure_update_allowed().is_err(),
    )
}

async fn cleanup_refused_content_exit(services: DesktopServices) {
    services.installs.close_admission();
    services
        .tasks
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), services.installs.join_observers())
        .await
        .unwrap()
        .unwrap();
    services.installs.close_events();
    services.server.shutdown.send_replace(true);
    services.server.wait().await.unwrap();
    ServerHandle::join_owned(&services.server.telemetry_worker, "fixture telemetry join")
        .await
        .unwrap();
    ServerHandle::join_owned(&services.server.rules_worker, "fixture rules join")
        .await
        .unwrap();
    services.server.runtime_cache.settle().unwrap();
    services.music.settle().unwrap();
    let library = services.library.clone();
    drop(services);
    library.try_preserve().unwrap();
}

#[tokio::test]
async fn interrupted_content_blocks_reset_but_allows_preserved_exit() {
    interrupted_content_exit(ContentExit::Acknowledgement).await;
}

#[tokio::test]
async fn interrupted_queued_content_blocks_reset_but_allows_preserved_exit() {
    interrupted_content_exit(ContentExit::QueuedAcknowledgement).await;
}

#[tokio::test]
async fn interrupted_content_public_moves_preserve_fences_and_allow_exit() {
    interrupted_content_exit(ContentExit::PublicMoves).await;
}

enum ContentExit {
    Acknowledgement,
    QueuedAcknowledgement,
    PublicMoves,
}

async fn interrupted_content_exit(boundary: ContentExit) {
    use std::io::{Seek, SeekFrom};

    let queued = matches!(boundary, ContentExit::QueuedAcknowledgement);
    let public_moves = matches!(boundary, ContentExit::PublicMoves);
    let temporary = temporary();
    let root = admit_profile(&temporary.path().join("replacement"))
        .unwrap()
        .root;
    let identity = fs::read(root.join(PROFILE_MARKER)).unwrap();
    let mut output = tempfile::tempfile().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "library_tests::pending_content_exit_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(PENDING_CONTENT_PROFILE, &root)
        .env(PENDING_CONTENT_QUEUED, if queued { "1" } else { "0" })
        .env(
            PENDING_CONTENT_PUBLIC_MOVE,
            if public_moves { "1" } else { "0" },
        )
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
        Some(PENDING_CONTENT_EXIT),
        "{tail}"
    );

    let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
    let records = axial_app::instances::directory::Registry::new(storage.clone())
        .list()
        .unwrap();
    assert_eq!(records.len(), 1);
    let id = records[0].instance.id.clone();
    let payload = root.join("instances").join(&records[0].directory_name);
    let receipt = content_receipt(&storage, &id);
    let tree = content_tree(&payload);
    let queue_identity = content_queue_rows(&storage, false);
    assert_eq!(queue_identity.len(), usize::from(queued));
    assert_eq!(
        serde_json::from_str::<Value>(&receipt.1).unwrap()["native_settled"],
        false
    );
    assert_eq!(
        fs::read(payload.join("config/preserved.txt")).unwrap(),
        CONTENT_OVERRIDE
    );
    if public_moves {
        assert_eq!(
            fs::read(payload.join(CONTENT_PUBLIC_PATH)).unwrap(),
            CONTENT_PUBLIC_BYTES
        );
        let recorded: Value = serde_json::from_str(&receipt.1).unwrap();
        assert_eq!(
            fs::read(payload.join(MANIFEST_FILE)).unwrap(),
            serde_json::from_value::<Vec<u8>>(recorded["before_manifest"].clone()).unwrap()
        );
    }
    drop(storage);

    let mut observations = Vec::new();
    for _ in 0..2 {
        let services = start_in_profile(root.clone(), None).await.unwrap();
        let bootstrap = services.server.bootstrap();
        let before = (
            content_receipt(services.instances.registry().storage(), &id),
            content_tree(&payload),
        );
        let queue_before = content_queue_rows(services.instances.registry().storage(), true);
        assert_eq!(
            content_queue_rows(services.instances.registry().storage(), false),
            queue_identity
        );
        let queue_status = services.installs.snapshot().active.map(|active| {
            (
                active.queue_id.clone(),
                serde_json::to_value(services.installs.status(&active.queue_id).unwrap()).unwrap(),
            )
        });
        let before_fence = content_fence(&services, &id).await;
        let response = reqwest::Client::new()
            .post(format!(
                "{}/api/v1/instances/{id}/content/settle",
                bootstrap.base_url
            ))
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .send()
            .await
            .unwrap();
        let settle_status = response.status().as_u16();
        let settle_body: Value = response.json().await.unwrap();
        let after_fence = content_fence(&services, &id).await;
        let after = (
            content_receipt(services.instances.registry().storage(), &id),
            content_tree(&payload),
        );
        let idle = services.tasks.status().is_idle();
        let shutdown = services.server.shutdown().await;
        let shutdown_settled = services.server.is_shutdown_settled();
        let joined = services.tasks.shutdown_receipt().is_some();
        let reset_refused = services.server.ensure_reset_settled().is_err();
        let queue_after = content_queue_rows(services.instances.registry().storage(), true);
        let queue_after_status = services.installs.snapshot().active.map(|active| {
            (
                active.queue_id.clone(),
                serde_json::to_value(services.installs.status(&active.queue_id).unwrap()).unwrap(),
            )
        });
        let http_closed = reqwest::Client::new()
            .get(format!("{}/api/v1/status", bootstrap.base_url))
            .header(transport::CAPABILITY_HEADER, &bootstrap.capability)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_err();
        let preserved = (
            content_receipt(services.instances.registry().storage(), &id),
            content_tree(&payload),
        );
        if shutdown.is_err() {
            // Teardown does not acknowledge historical Content or mark shutdown settled.
            cleanup_refused_content_exit(services).await;
        } else {
            drop(services);
        }
        observations.push((
            (
                shutdown,
                shutdown_settled,
                joined,
                http_closed,
                idle,
                reset_refused,
            ),
            (before, after, preserved),
            (before_fence, after_fence),
            (settle_status, settle_body),
            (queue_before, queue_after, queue_status, queue_after_status),
        ));
    }
    assert_eq!(fs::read(root.join(PROFILE_MARKER)).unwrap(), identity);
    let storage = MetadataStore::open(root.join("metadata.sqlite")).unwrap();
    assert_eq!(content_receipt(&storage, &id), receipt);
    assert_eq!(content_tree(&payload), tree);
    for (
        (shutdown, settled, joined, http_closed, idle, reset_refused),
        (before, after, preserved),
        (before_fence, after_fence),
        (settle_status, settle_body),
        (queue_before, queue_after, queue_status, queue_after_status),
    ) in observations
    {
        assert_eq!(before, (receipt.clone(), tree.clone()));
        assert_eq!(after, before);
        assert_eq!(preserved, before);
        assert_eq!(settle_status, 409);
        assert_eq!(
            settle_body,
            json!({"error":"content operation requires settlement before this instance can be used"})
        );
        assert_eq!(before_fence.0, 503);
        assert_eq!(after_fence.0, 503);
        assert!(before_fence.1 && after_fence.1);
        assert!(idle && joined);
        assert_eq!(queue_after, queue_before);
        assert_eq!(queue_after_status, queue_status);
        assert_eq!(queue_status.is_some(), queued);
        if let Some((_, status)) = queue_status {
            assert_eq!(status["view_model"]["phase_id"], "settlement_required");
            assert_eq!(status["done"], false);
            assert!(status["outcome"].is_null());
        }
        assert_eq!(
            shutdown,
            Ok(()),
            "historical Content must permit preserve-only exit"
        );
        assert!(settled && http_closed);
        assert!(reset_refused, "quiescent reset must retain durable Content");
        assert!(
            before_fence.2 && after_fence.2,
            "destructive reset must retain the Content fence"
        );
        assert!(
            before_fence.3 && after_fence.3,
            "update must retain the Content fence"
        );
    }
}

#[tokio::test]
#[ignore = "subprocess helper exiting at a real Content publication boundary"]
async fn pending_content_exit_helper() {
    let root = PathBuf::from(std::env::var_os(PENDING_CONTENT_PROFILE).unwrap());
    assert_eq!(fs::canonicalize(&root).unwrap(), root);
    let services = start_in_profile(root.clone(), None).await.unwrap();
    let queued = std::env::var(PENDING_CONTENT_QUEUED).unwrap() == "1";
    let public_moves = std::env::var(PENDING_CONTENT_PUBLIC_MOVE).unwrap() == "1";
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
                name: "Interrupted content instance".into(),
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
    let storage = services.instances.registry().storage();
    let service =
        ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap()).unwrap();
    if queued || public_moves {
        services
            .content_mutations
            .install_pack(&service, &instance.id, override_pack(), true)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
    }
    if public_moves {
        use axial_minecraft::managed_path::{
            ManagedContentStagingCheckpoint, ManagedContentTransactionRoot,
        };

        let record = services
            .instances
            .registry()
            .get_record(&instance.id)
            .unwrap();
        let payload = root.join("instances").join(record.directory_name);
        let before_manifest = fs::read(payload.join(MANIFEST_FILE)).unwrap();
        let observed_payload = payload.clone();
        let observed_registry = services.instances.registry().clone();
        let id = instance.id.clone();
        ManagedContentTransactionRoot::before_manifest_revalidation_for_test(&payload, move || {
            let receipt = content_receipt(observed_registry.storage(), &id);
            let recorded: Value = serde_json::from_str(&receipt.1).unwrap();
            assert_eq!(recorded["native_settled"], false);
            ManagedContentStagingCheckpoint::decode(recorded["ready_checkpoint"].as_str().unwrap())
                .unwrap();
            assert_eq!(recorded["changes"].as_array().unwrap().len(), 1);
            assert_eq!(recorded["changes"][0]["path"], CONTENT_PUBLIC_PATH);
            assert_eq!(
                serde_json::from_value::<Vec<u8>>(recorded["before_manifest"].clone()).unwrap(),
                before_manifest
            );
            assert_ne!(recorded["before_manifest"], recorded["after_manifest"]);
            assert_eq!(
                fs::read(observed_payload.join(MANIFEST_FILE)).unwrap(),
                before_manifest
            );
            assert_eq!(
                fs::read(observed_payload.join("config/preserved.txt")).unwrap(),
                CONTENT_OVERRIDE
            );
            assert_eq!(
                fs::read(observed_payload.join(CONTENT_PUBLIC_PATH)).unwrap(),
                CONTENT_PUBLIC_BYTES
            );
            content_tree(&observed_payload);
            std::process::exit(PENDING_CONTENT_EXIT);
        })
        .unwrap();
        let result = services
            .content_mutations
            .install_pack(
                &service,
                &instance.id,
                override_pack_at(CONTENT_PUBLIC_PATH, CONTENT_PUBLIC_BYTES),
                true,
            )
            .unwrap()
            .join()
            .await
            .unwrap();
        panic!("Content must exit after public moves and before manifest publication: {result:?}");
    }
    storage.transaction(|db| -> Result<(), StorageError> {
        db.execute_batch("CREATE TRIGGER refuse_content_acknowledgement BEFORE UPDATE ON content_batches WHEN json_extract(NEW.receipt_json,'$.native_settled') = 1 BEGIN SELECT RAISE(ABORT, 'injected content acknowledgement refusal'); END;")?;
        Ok(())
    }).unwrap();
    if queued {
        let bootstrap = services.server.bootstrap();
        let response = reqwest::Client::new()
            .post(format!(
                "{}/api/v1/instances/{}/content/uninstall",
                bootstrap.base_url, instance.id
            ))
            .header(transport::CAPABILITY_HEADER, bootstrap.capability)
            .json(&json!({"canonical_ids":["modrinth:fixture-pack"]}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let response: Value = response.json().await.unwrap();
        let id = response["started_install"]["install_id"].as_str().unwrap();
        let (_, mut changes) = services.installs.subscribe();
        tokio::time::timeout(Duration::from_secs(10), async {
            while services.installs.status(id).unwrap().view_model.phase_id != "settlement_required"
            {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    } else {
        assert!(matches!(
            services
                .content_mutations
                .install_pack(&service, &instance.id, override_pack(), true)
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(MutationError::Pending)
        ));
    }
    services.installs.close_admission();
    services
        .tasks
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
    services.installs.join_observers().await.unwrap();
    assert!(services.tasks.shutdown_receipt().is_some());
    if queued {
        assert!(
            services
                .installs
                .preserve_shutdown(&services.tasks.shutdown_receipt().unwrap())
                .is_err()
        );
    }
    let receipt = content_receipt(storage, &instance.id);
    let recorded: Value = serde_json::from_str(&receipt.1).unwrap();
    assert_eq!(recorded["native_settled"], false);
    let record = services
        .instances
        .registry()
        .get_record(&instance.id)
        .unwrap();
    let payload = root.join("instances").join(record.directory_name);
    assert_eq!(
        fs::read(payload.join("config/preserved.txt")).unwrap(),
        CONTENT_OVERRIDE
    );
    let manifest = fs::read(payload.join(MANIFEST_FILE)).unwrap();
    assert_eq!(
        manifest,
        serde_json::from_value::<Vec<u8>>(recorded["after_manifest"].clone()).unwrap()
    );
    let pack = CanonicalId::for_project(ProviderId::Modrinth, "fixture-pack");
    assert_eq!(
        ContentManifest::decode_managed(Some(&manifest))
            .unwrap()
            .find(&pack)
            .is_some(),
        !queued
    );
    if queued {
        let before =
            serde_json::from_value::<Vec<u8>>(recorded["before_manifest"].clone()).unwrap();
        assert!(
            ContentManifest::decode_managed(Some(&before))
                .unwrap()
                .find(&pack)
                .is_some()
        );
    }
    storage
        .transaction(|db| -> Result<(), StorageError> {
            db.execute_batch("DROP TRIGGER refuse_content_acknowledgement;")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(content_receipt(storage, &instance.id), receipt);
    // This loses the retained owner after the SQL refusal, not during native staging.
    std::process::exit(PENDING_CONTENT_EXIT);
}
