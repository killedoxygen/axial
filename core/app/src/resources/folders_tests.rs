use super::*;
use crate::{
    instances::{
        create::{CreateInstanceRequest, CreateTarget},
        directory::{InstanceDirectories, Registry},
    },
    library::{LibraryLifecycle, LibraryOpenOutcome},
    storage::MetadataStore,
    tasks::Exclusions,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct ProcessControl {
    exited: AtomicBool,
    terminated: AtomicBool,
    reaped: AtomicUsize,
    wait_error: AtomicBool,
}

struct FakeProcess(Arc<ProcessControl>);

impl FolderProcess for FakeProcess {
    fn try_wait(&mut self) -> io::Result<bool> {
        if self.0.wait_error.load(Ordering::SeqCst) {
            return Err(io::Error::other("fixture wait failure"));
        }
        let exited =
            self.0.exited.load(Ordering::SeqCst) || self.0.terminated.load(Ordering::SeqCst);
        if exited {
            self.0.reaped.fetch_add(1, Ordering::SeqCst);
        }
        Ok(exited)
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.0.terminated.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingOpener {
    paths: Mutex<Vec<PathBuf>>,
    process: Arc<ProcessControl>,
    fail: AtomicBool,
}

impl FolderOpener for RecordingOpener {
    fn spawn(&self, folder: &AdmittedFolder) -> Result<Box<dyn FolderProcess>, FolderError> {
        let path = folder.checked_path()?;
        self.paths.lock().unwrap().push(path);
        if self.fail.load(Ordering::SeqCst) {
            return Err(FolderError::Spawn);
        }
        Ok(Box::new(FakeProcess(self.process.clone())))
    }
}

struct Fixture {
    root: tempfile::TempDir,
    instances: Arc<InstanceService>,
    tasks: TaskOwner,
    service: FolderService,
    opener: Arc<RecordingOpener>,
    id: InstanceId,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("fixture library: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::create::DUPLICATE_WITNESS_MIGRATION,
                crate::instances::delete::MIGRATION,
                crate::content::install::MIGRATION,
                crate::performance::mutation::MIGRATION,
                crate::performance::mutation::MIGRATION_V2,
            ])
            .unwrap();
        let tasks = TaskOwner::new(16).unwrap();
        let instances = Arc::new(InstanceService::new(
            InstanceDirectories::new(Registry::new(storage), library, Exclusions::new()),
            tasks.clone(),
        ));
        let opener = Arc::new(RecordingOpener::default());
        let id = create(&instances, "Folder fixture").await;
        let service = FolderService::new(instances.clone(), tasks.clone(), opener.clone());
        Self {
            root,
            instances,
            tasks,
            service,
            opener,
            id,
        }
    }

    fn game(&self) -> PathBuf {
        self.root.path().join("instances").join(self.id.as_str())
    }
}

async fn create(instances: &InstanceService, name: &str) -> InstanceId {
    instances
        .create(
            CreateInstanceRequest {
                name: name.into(),
                selection_id: "vanilla|1.21.4".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "vanilla|1.21.4".into(),
                version_id: "1.21.4".into(),
                minecraft_version: "1.21.4".into(),
                loader_key: "vanilla".into(),
            },
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap()
        .id
}

async fn idle(tasks: &TaskOwner) {
    let mut changed = tasks.subscribe();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !tasks.status().is_idle() {
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("folder task settles");
}

#[tokio::test]
async fn root_and_each_allowed_folder_open_only_the_registered_projection() {
    let fixture = Fixture::new().await;
    fixture.opener.process.exited.store(true, Ordering::SeqCst);
    std::fs::write(fixture.game().join("user-file.txt"), b"preserved").unwrap();
    for sub in std::iter::once(None).chain(SUBFOLDERS.into_iter().map(Some)) {
        if let Some(sub) = sub {
            let path = fixture.game().join(sub);
            // All fixture subfolders are empty. Prove creation, then reopen it.
            if path.exists() {
                std::fs::remove_dir(path).unwrap();
            }
        }
        for _ in 0..2 {
            fixture
                .service
                .open(&fixture.id, sub)
                .unwrap()
                .wait()
                .await
                .unwrap();
            idle(&fixture.tasks).await;
            let expected = match sub {
                Some(sub) => fixture.game().join(sub),
                None => fixture.game(),
            };
            assert!(expected.is_dir());
            assert_eq!(fixture.opener.paths.lock().unwrap().last(), Some(&expected));
        }
    }
    assert_eq!(
        std::fs::read(fixture.game().join("user-file.txt")).unwrap(),
        b"preserved"
    );
    assert_eq!(fixture.opener.paths.lock().unwrap().len(), 16);
}

#[tokio::test]
async fn missing_instance_and_path_selectors_never_reach_the_opener() {
    let fixture = Fixture::new().await;
    assert!(matches!(
        fixture.service.open(&InstanceId::new(), Some("../outside")),
        Err(FolderError::NotFound)
    ));
    for sub in [
        "",
        "..",
        "../outside",
        "/tmp",
        "mods/",
        "./mods",
        "Mods",
        "C:\\Windows",
        "file:///tmp",
    ] {
        assert!(
            matches!(
                fixture.service.open(&fixture.id, Some(sub)),
                Err(FolderError::InvalidFolder)
            ),
            "selector: {sub:?}"
        );
    }
    assert!(fixture.opener.paths.lock().unwrap().is_empty());
    assert!(!fixture.root.path().join("outside").exists());
    assert!(fixture.tasks.status().is_idle());
}

#[tokio::test]
async fn file_collision_and_busy_instance_do_not_spawn_an_opener() {
    let fixture = Fixture::new().await;
    let logs = fixture.game().join("logs");
    if logs.exists() {
        std::fs::remove_dir(&logs).unwrap();
    }
    std::fs::write(&logs, b"not a folder").unwrap();
    assert_eq!(
        fixture
            .service
            .open(&fixture.id, Some("logs"))
            .unwrap()
            .wait()
            .await,
        Err(FolderError::Prepare)
    );
    idle(&fixture.tasks).await;
    assert_eq!(std::fs::read(logs).unwrap(), b"not a folder");
    let busy = fixture.instances.directories().admit(&fixture.id).unwrap();
    assert!(matches!(
        fixture.service.open(&fixture.id, None),
        Err(FolderError::Busy)
    ));
    assert!(fixture.opener.paths.lock().unwrap().is_empty());
    drop(busy);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_folder_cannot_open_or_modify_an_external_directory() {
    let fixture = Fixture::new().await;
    let external = tempfile::tempdir().unwrap();
    let logs = fixture.game().join("logs");
    if logs.exists() {
        std::fs::remove_dir(&logs).unwrap();
    }
    std::fs::write(external.path().join("keep.txt"), b"external").unwrap();
    std::os::unix::fs::symlink(external.path(), &logs).unwrap();
    assert_eq!(
        fixture
            .service
            .open(&fixture.id, Some("logs"))
            .unwrap()
            .wait()
            .await,
        Err(FolderError::Prepare)
    );
    idle(&fixture.tasks).await;
    assert!(fixture.opener.paths.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read(external.path().join("keep.txt")).unwrap(),
        b"external"
    );
    assert!(
        std::fs::symlink_metadata(logs)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn failed_spawn_reports_failure_and_releases_admission() {
    let fixture = Fixture::new().await;
    fixture.opener.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        fixture
            .service
            .open(&fixture.id, None)
            .unwrap()
            .wait()
            .await,
        Err(FolderError::Spawn)
    );
    idle(&fixture.tasks).await;
    fixture.instances.directories().admit(&fixture.id).unwrap();
    assert_eq!(fixture.opener.process.reaped.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dropped_waiter_retains_exclusion_and_generation_until_shutdown_reaps() {
    let fixture = Fixture::new().await;
    drop(fixture.service.open(&fixture.id, None).unwrap());
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.opener.paths.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        fixture.instances.directories().admit(&fixture.id),
        Err(InstanceError::Busy)
    ));
    assert!(
        fixture
            .instances
            .directories()
            .library()
            .snapshot()
            .current
            .unwrap()
            .pins
            > 0
    );
    fixture
        .tasks
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(fixture.opener.process.terminated.load(Ordering::SeqCst));
    assert_eq!(fixture.opener.process.reaped.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .instances
            .directories()
            .library()
            .snapshot()
            .current
            .unwrap()
            .pins,
        0
    );
    assert!(matches!(
        fixture.service.open(&fixture.id, Some("logs")),
        Err(FolderError::Closed)
    ));
}

#[tokio::test]
async fn uncertain_process_wait_preserves_admission_and_shutdown_refusal_until_reap() {
    let fixture = Fixture::new().await;
    fixture
        .opener
        .process
        .wait_error
        .store(true, Ordering::SeqCst);
    fixture
        .service
        .open(&fixture.id, None)
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(
        fixture
            .tasks
            .shutdown(Duration::from_millis(5))
            .await
            .is_err()
    );
    assert!(matches!(
        fixture.instances.directories().admit(&fixture.id),
        Err(InstanceError::Busy)
    ));
    fixture
        .opener
        .process
        .wait_error
        .store(false, Ordering::SeqCst);
    fixture
        .tasks
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(fixture.opener.process.reaped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn active_folder_openers_are_bounded_across_instances() {
    let fixture = Fixture::new().await;
    let mut ids = vec![fixture.id.clone()];
    for index in 1..=MAX_OPENERS {
        ids.push(create(&fixture.instances, &format!("Folder {index}")).await);
    }
    for id in &ids[..MAX_OPENERS] {
        fixture
            .service
            .open(id, None)
            .unwrap()
            .wait()
            .await
            .unwrap();
    }
    assert!(matches!(
        fixture.service.open(&ids[MAX_OPENERS], None),
        Err(FolderError::Capacity)
    ));
    assert_eq!(fixture.opener.paths.lock().unwrap().len(), MAX_OPENERS);
    fixture
        .tasks
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(
        fixture.opener.process.reaped.load(Ordering::SeqCst),
        MAX_OPENERS
    );
}

struct SubstitutingOpener(Arc<ProcessControl>);

impl FolderOpener for SubstitutingOpener {
    fn spawn(&self, folder: &AdmittedFolder) -> Result<Box<dyn FolderProcess>, FolderError> {
        let path = folder.checked_path()?;
        std::fs::rename(&path, path.with_extension("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        Ok(Box::new(FakeProcess(self.0.clone())))
    }
}

#[tokio::test]
async fn a_folder_replaced_during_spawn_is_not_reported_as_opened_and_process_is_reaped() {
    let fixture = Fixture::new().await;
    let control = Arc::new(ProcessControl::default());
    let service = FolderService::new(
        fixture.instances.clone(),
        fixture.tasks.clone(),
        Arc::new(SubstitutingOpener(control.clone())),
    );
    assert_eq!(
        service
            .open(&fixture.id, Some("logs"))
            .unwrap()
            .wait()
            .await,
        Err(FolderError::Prepare)
    );
    idle(&fixture.tasks).await;
    assert!(control.terminated.load(Ordering::SeqCst));
    assert_eq!(control.reaped.load(Ordering::SeqCst), 1);
    assert!(fixture.game().join("logs.original").is_dir());
    assert!(fixture.game().join("logs").is_dir());
}
