use super::*;

fn fixture_directory() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

fn open_storage(path: &std::path::Path) -> Arc<MetadataStore> {
    let storage = Arc::new(MetadataStore::open(path).unwrap());
    storage.migrate(&[MIGRATION]).unwrap();
    storage
}

fn service(root: &std::path::Path, storage: Arc<MetadataStore>) -> BenchmarkService {
    service_with_directories(root, storage).0
}

fn service_with_directories(
    root: &std::path::Path,
    storage: Arc<MetadataStore>,
) -> (
    BenchmarkService,
    crate::instances::directory::InstanceDirectories,
) {
    use crate::{
        accounts::{
            credential_store::CredentialStore, directory::AccountDirectory, session::AuthService,
        },
        content::catalog::ContentService,
        install::queue::InstallQueue,
        instances::directory::InstanceDirectories,
        library::{LibraryLifecycle, LibraryOpenOutcome},
        network::{ClientConfig, ProviderClient},
        performance::PerformanceService,
        runtime::discovery::RuntimeDiscovery,
        settings::SettingsStore,
        skins::{ProfileMedia, library::SavedSkinLibrary, store::SavedSkinStore},
        tasks::Exclusions,
    };
    storage
        .migrate(&[
            crate::instances::directory::MIGRATION,
            crate::instances::create::MIGRATION,
            crate::content::install::MIGRATION,
            crate::install::queue::MIGRATION,
            crate::performance::rules::MIGRATION,
            crate::performance::mutation::MIGRATION,
            crate::skins::store::MIGRATION,
            crate::launch::coordinator::INTENT_MIGRATION,
        ])
        .unwrap();
    let library = match LibraryLifecycle::open(root) {
        LibraryOpenOutcome::Ready(library) => library,
        other => panic!("fixture library unavailable: {other:?}"),
    };
    let registry = Registry::new(storage.clone());
    let directories =
        InstanceDirectories::new(registry.clone(), library.clone(), Exclusions::new());
    let tasks = TaskOwner::new(16).unwrap();
    let runtime = axial_minecraft::ManagedRuntimeCache::isolated_for_test().unwrap();
    let installs = InstallQueue::new(
        storage.clone(),
        library.clone(),
        directories.exclusions().clone(),
        tasks.clone(),
        runtime.clone(),
    )
    .unwrap();
    let settings = Arc::new(SettingsStore::new(storage.clone()).unwrap());
    let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
    let auth = Arc::new(AuthService::new(
        accounts.clone(),
        Arc::new(CredentialStore::isolated_for_tests()),
        tasks.clone(),
    ));
    let client = ProviderClient::new(ClientConfig::default()).unwrap();
    let content = Arc::new(ContentService::new(client).unwrap());
    let performance = PerformanceService::new(
        storage.clone(),
        directories.clone(),
        tasks.clone(),
        content,
        super::super::public_transfer_resolver(),
    )
    .unwrap();
    let pin = library.admit_application_root().unwrap();
    let skins = ProfileMedia::new(
        Arc::new(SavedSkinLibrary::new(
            SavedSkinStore::new(storage.clone()),
            pin.clone(),
        )),
        accounts.clone(),
        auth.clone(),
        tasks.clone(),
        pin,
    )
    .unwrap();
    let reports = LaunchReportStore::new(storage.clone()).unwrap();
    let sessions = SessionManager::with_reports(tasks.clone(), reports.clone());
    let launches = LaunchCoordinator::new(
        directories.clone(),
        accounts,
        settings,
        installs,
        RuntimeDiscovery::new(runtime, tasks.clone()),
        performance,
        auth,
        skins,
        sessions.clone(),
        tasks.clone(),
    )
    .with_storage(storage.clone(), reports.clone())
    .unwrap();
    let service = BenchmarkService::new(
        storage,
        registry,
        Arc::new(reports),
        launches,
        sessions,
        tasks,
    )
    .unwrap();
    (service, directories)
}

async fn instance_service(
    root: &std::path::Path,
    storage: Arc<MetadataStore>,
) -> (BenchmarkService, InstanceId) {
    use crate::instances::create::{CreateInstanceRequest, CreateTarget, InstanceService};
    let (service, directories) = service_with_directories(root, storage);
    let instances = InstanceService::new(directories, service.tasks.clone());
    let instance = instances
        .create(
            CreateInstanceRequest {
                name: "Benchmark fixture".into(),
                selection_id: "1.21.1".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "1.21.1".into(),
                version_id: "1.21.1".into(),
                minecraft_version: "1.21.1".into(),
                loader_key: "vanilla".into(),
            },
            instances.creation_admission_for_tests().await.unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    (service, instance.id)
}

async fn persist_automatic_restart_fixture(
    service: &BenchmarkService,
    instance: &InstanceId,
    count: usize,
) -> Vec<BenchmarkSuiteDriverStatus> {
    let mut accepted = Vec::new();
    for index in 0..count {
        let input = serde_json::from_value(serde_json::json!({
            "instance_id": instance, "suite_id": format!("restart-suite-{index}"),
            "suite_mode": "development", "interval_ms": 5_000
        }))
        .unwrap();
        accepted.push(service.start_driver(input).await.unwrap());
    }
    // Preserve the actual accepted pre-tick rows as the restart fixture.
    // Stop the fixture's retained tasks before restoring that crash boundary.
    for driver in &accepted {
        service.stop_driver(&driver.id).unwrap();
    }
    service
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    service
        .storage
        .transaction(|tx| {
            assert_eq!(
                tx.query_row("SELECT count(*) FROM launch_intents", [], |row| row
                    .get::<_, usize>(0))?,
                0
            );
            for driver in &accepted {
                assert_eq!(
                    tx.execute(
                        "UPDATE benchmark_drivers SET payload=?1 WHERE driver_id=?2",
                        params![serde_json::to_vec(driver).unwrap(), driver.id],
                    )?,
                    1
                );
            }
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    accepted
}

#[tokio::test]
async fn refused_resume_preserves_stopped_driver_while_original_task_settles() {
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    let (service, instance) = instance_service(root.path(), storage.clone()).await;
    let accepted = service
        .start_driver(
            serde_json::from_value(serde_json::json!({
                "instance_id": instance, "suite_mode": "development", "interval_ms": 300_000
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service.stop_driver(&accepted.id)?;
        let stopped = service.driver(&accepted.id)?;
        let records = || {
            storage.read(|db| {
            db.query_row(
                "SELECT d.payload,d.request,s.payload FROM benchmark_drivers d JOIN benchmark_suites s ON s.suite_id=?2 WHERE d.driver_id=?1",
                params![accepted.id, accepted.suite_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?)),
            ).map_err(BenchmarkError::from)
        })
        };
        let before = records()?;
        let ownership = || {
            let cancelled = service
                .active_drivers
                .lock()
                .unwrap()
                .get(&accepted.id)
                .map(CancellationToken::is_cancelled);
            (cancelled, service.tasks.status())
        };
        let retained = ownership();
        let resumed = service.resume_driver(&accepted.id);
        let remaining = ownership();
        let after = records()?;
        let current = service.driver(&accepted.id)?;
        let sessions = service.sessions.sessions();
        Ok::<_, BenchmarkError>((
            stopped, before, retained, resumed, remaining, after, current, sessions,
        ))
    }));
    if service
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .is_err()
    {
        std::mem::forget(service);
        std::mem::forget(storage);
        std::mem::forget(root);
        panic!("benchmark fixture shutdown did not settle");
    }
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (stopped, before, retained, resumed, remaining, after, current, sessions) =
            observed.unwrap().unwrap();
        assert_eq!(stopped.state, "stopped");
        assert_eq!(retained.0, Some(true));
        assert_eq!(retained.1.running.len(), 1);
        assert_eq!(
            remaining, retained,
            "refusal must retain the original cancelled task"
        );
        assert!(matches!(resumed, Err(BenchmarkError::Busy)));
        assert_eq!(
            current, stopped,
            "a refused resume must not publish running"
        );
        assert_eq!(
            after, before,
            "refusal must preserve driver, request and suite bytes"
        );
        assert!(sessions.is_empty());
        assert!(service.tasks.status().is_idle());
        assert!(service.active_drivers.lock().unwrap().is_empty());
    }));
    if let Err(failure) = checked {
        use std::io::Write;
        let retained = root.keep();
        let _ = writeln!(
            std::io::stderr(),
            "benchmark refusal fixture retained at {}",
            retained.display()
        );
        std::panic::resume_unwind(failure);
    }
}

#[tokio::test]
async fn graceful_driver_shutdown_stops_and_joins_before_reopen() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let (service, instance) = instance_service(root.path(), storage.clone()).await;
    let input = serde_json::from_value(serde_json::json!({
        "instance_id": instance, "suite_mode": "development", "interval_ms": 300_000
    }))
    .unwrap();
    let accepted = service.start_driver(input).await.unwrap();
    let suite = service.suite(&accepted.suite_id).unwrap().unwrap();
    let request = storage
        .read(|db| -> Result<Vec<u8>, StorageError> {
            Ok(db.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [&accepted.id],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(accepted.state, "running");
    assert_eq!(service.tasks.status().running.len(), 1);

    // Cancellation reaches the accepted driver before its first tick on this
    // current-thread runtime; no missing install can stand in for shutdown.
    service
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    assert!(service.tasks.status().is_idle());
    assert!(service.tasks.status().closing);
    assert!(service.active_drivers.lock().unwrap().is_empty());
    let stopped = service.driver(&accepted.id).unwrap();
    let mut expected = accepted.clone();
    expected.state = "stopped".into();
    expected.updated_at = stopped.updated_at.clone();
    assert_eq!(stopped, expected);
    assert_eq!(
        service.suite(&accepted.suite_id).unwrap(),
        Some(suite.clone())
    );
    assert!(service.sessions.sessions().is_empty());
    drop(service);
    drop(storage);

    let storage = open_storage(&path);
    let reopened = self::service(root.path(), storage.clone());
    assert_eq!(reopened.driver(&accepted.id).unwrap(), stopped);
    assert_eq!(reopened.suite(&accepted.suite_id).unwrap(), Some(suite));
    assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
    assert!(reopened.can_resume_driver(&accepted.id).unwrap());
    assert!(reopened.tasks.status().is_idle());
    storage
        .read(|db| -> Result<(), StorageError> {
            let saved: Vec<u8> = db.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [&accepted.id],
                |row| row.get(0),
            )?;
            assert_eq!(saved, request);
            assert_eq!(
                db.query_row("SELECT count(*) FROM launch_intents", [], |row| row
                    .get::<_, usize>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn current_driver_history_reopens_and_resumes_the_same_driver() {
    for state in ["stopped", "failed", "interrupted"] {
        let root = fixture_directory();
        let path = root.path().join("metadata.sqlite");
        let storage = open_storage(&path);
        let (service, instance) = instance_service(root.path(), storage.clone()).await;
        let input = serde_json::from_value(serde_json::json!({
            "instance_id": instance, "suite_mode": "development", "interval_ms": 5_000
        }))
        .unwrap();
        let accepted = service.start_driver(input).await.unwrap();
        service.stop_driver(&accepted.id).unwrap();
        service
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let mut driver = service.driver(&accepted.id).unwrap();
        driver.state = state.into();
        driver.error = (state != "stopped").then(|| "Benchmark work stopped before launch".into());
        service.save_driver(&driver).unwrap();
        let persisted = service.driver(&driver.id).unwrap();
        let suite = service.suite(&driver.suite_id).unwrap().unwrap();
        drop(service);
        drop(storage);

        let storage = open_storage(&path);
        let reopened = self::service(root.path(), storage.clone());
        assert_eq!(reopened.driver(&driver.id).unwrap(), persisted);
        assert_eq!(
            reopened.suite(&suite.suite_id).unwrap(),
            Some(suite.clone())
        );
        assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
        assert!(reopened.can_resume_driver(&driver.id).unwrap());
        let resumed = reopened.resume_driver(&driver.id).unwrap();
        assert_eq!(resumed.id, driver.id);
        assert_eq!(resumed.suite_id, suite.suite_id);
        assert_eq!(resumed.state, "running");
        assert_eq!(resumed.error, None);
        assert!(!reopened.can_resume_driver(&driver.id).unwrap());
        assert!(matches!(
            reopened.resume_driver(&driver.id),
            Err(BenchmarkError::Busy)
        ));
        reopened.stop_driver(&driver.id).unwrap();
        reopened
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        storage
            .read(|db| {
                assert_eq!(stored_drivers_in(db)?.len(), 1);
                assert_eq!(stored_suite(db, &suite.suite_id)?, Some(suite.clone()));
                assert_eq!(
                    db.query_row("SELECT count(*) FROM launch_intents", [], |row| row
                        .get::<_, usize>(0))?,
                    0
                );
                Ok::<_, BenchmarkError>(())
            })
            .unwrap();
    }
}

#[tokio::test]
async fn current_driver_capacity_refuses_new_work_without_pruning_history() {
    for count in [MAX_STORED_DRIVERS - 1, MAX_STORED_DRIVERS] {
        let root = fixture_directory();
        let path = root.path().join("metadata.sqlite");
        let storage = open_storage(&path);
        let (service, instance) = instance_service(root.path(), storage.clone()).await;
        let input = serde_json::from_value(serde_json::json!({
            "instance_id": instance, "suite_id": "retained-suite", "suite_mode": "development"
        }))
        .unwrap();
        let accepted = service.start_driver(input).await.unwrap();
        service.stop_driver(&accepted.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !service.tasks.status().is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let retained = service.driver(&accepted.id).unwrap();
        storage
            .transaction(|tx| {
                let request: Vec<u8> = tx.query_row(
                    "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                    [&retained.id],
                    |row| row.get(0),
                )?;
                for index in 1..count {
                    let mut driver = retained.clone();
                    driver.id = format!("retained-driver-{index}");
                    tx.execute(
                        "INSERT INTO benchmark_drivers(driver_id,payload,request) VALUES(?1,?2,?3)",
                        params![driver.id, serde_json::to_vec(&driver).unwrap(), request],
                    )?;
                }
                Ok::<_, BenchmarkError>(())
            })
            .unwrap();
        let input = serde_json::from_value(serde_json::json!({
            "instance_id": instance, "suite_id": "capacity-suite", "suite_mode": "development"
        }))
        .unwrap();
        service.ensure_suite(&input).unwrap();
        let result = service.start_driver(input).await;
        if count == MAX_STORED_DRIVERS {
            assert!(matches!(result, Err(BenchmarkError::Unavailable)));
        } else {
            service.stop_driver(&result.unwrap().id).unwrap();
        }
        service
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(service.driver(&retained.id).unwrap(), retained);
        storage
            .read(|db| {
                assert_eq!(stored_drivers_in(db)?.len(), MAX_STORED_DRIVERS);
                assert_eq!(
                    db.query_row("SELECT count(*) FROM launch_intents", [], |row| row
                        .get::<_, usize>(0))?,
                    0
                );
                Ok::<_, BenchmarkError>(())
            })
            .unwrap();
        drop(service);
        drop(storage);
        let reopened = self::service(root.path(), open_storage(&path));
        assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
        assert!(reopened.tasks.status().is_idle());
    }
}

#[tokio::test]
async fn automatic_restart_schedules_owned_driver_once() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let (service, instance) = instance_service(root.path(), storage.clone()).await;
    let accepted = persist_automatic_restart_fixture(&service, &instance, 1).await;
    drop(service);
    drop(storage);

    let storage = open_storage(&path);
    let reopened = self::service(root.path(), storage.clone());
    assert_eq!(
        reopened.driver(&accepted[0].id).unwrap().state,
        "interrupted"
    );
    assert_eq!(
        reopened.driver(&accepted[0].id).unwrap().error.as_deref(),
        Some(RESTART_INTERRUPTED_ERROR)
    );
    assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 1);
    assert_eq!(reopened.driver(&accepted[0].id).unwrap().state, "running");
    assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
    assert!(reopened.sessions.sessions().is_empty());
    reopened.stop_driver(&accepted[0].id).unwrap();
    reopened
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    drop(reopened);
    drop(storage);
    let reopened = self::service(root.path(), open_storage(&path));
    assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
    assert_eq!(reopened.driver(&accepted[0].id).unwrap().state, "stopped");
    assert!(reopened.tasks.status().is_idle());
}

#[tokio::test]
async fn automatic_restart_limit_is_durable_across_two_reopens() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let (service, instance) = instance_service(root.path(), storage.clone()).await;
    persist_automatic_restart_fixture(&service, &instance, 9).await;
    drop(service);
    drop(storage);

    let storage = open_storage(&path);
    let reopened = self::service(root.path(), storage.clone());
    assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 8);
    let statuses = reopened.drivers().unwrap();
    let limited = statuses
        .iter()
        .find(|driver| driver.state == "interrupted")
        .unwrap()
        .clone();
    for driver in statuses.iter().filter(|driver| driver.state == "running") {
        reopened.stop_driver(&driver.id).unwrap();
    }
    reopened
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    drop(reopened);
    drop(storage);
    assert_eq!(
        limited.error.as_deref(),
        Some("driver ignored after restart resume limit")
    );

    for _ in 0..2 {
        let reopened = self::service(root.path(), open_storage(&path));
        assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
        assert_eq!(reopened.driver(&limited.id).unwrap(), limited);
        assert!(reopened.tasks.status().is_idle());
        assert!(reopened.sessions.sessions().is_empty());
        assert!(reopened.can_resume_driver(&limited.id).unwrap());
    }
}

#[tokio::test]
async fn automatic_restart_invalid_driver_does_not_suppress_unrelated_work() {
    for case in [
        "missing_suite",
        "missing_request",
        "malformed_request",
        "wrong_binding",
    ] {
        let root = fixture_directory();
        let path = root.path().join("metadata.sqlite");
        let storage = open_storage(&path);
        let (service, instance) = instance_service(root.path(), storage.clone()).await;
        let accepted = persist_automatic_restart_fixture(&service, &instance, 2).await;
        let invalid = &accepted[1];
        storage.transaction(|tx| {
            match case {
                "missing_suite" => {
                    tx.execute("DELETE FROM benchmark_suites WHERE suite_id=?1", [&invalid.suite_id])?;
                }
                "missing_request" => {
                    tx.execute("UPDATE benchmark_drivers SET request=NULL WHERE driver_id=?1", [&invalid.id])?;
                }
                "malformed_request" => {
                    tx.execute("UPDATE benchmark_drivers SET request=?1 WHERE driver_id=?2", params![b"{".as_slice(), invalid.id])?;
                }
                "wrong_binding" => {
                    let request = serde_json::json!({"instance_id":instance,"suite_id":accepted[0].suite_id,"suite_mode":"development"});
                    tx.execute("UPDATE benchmark_drivers SET request=?1 WHERE driver_id=?2", params![serde_json::to_vec(&request).unwrap(), invalid.id])?;
                }
                _ => unreachable!(),
            }
            Ok::<_, BenchmarkError>(())
        }).unwrap();
        drop(service);
        drop(storage);
        let storage = open_storage(&path);
        let reopened = self::service(root.path(), storage.clone());
        assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 1, "{case}");
        let failed = reopened.driver(&invalid.id).unwrap();
        assert_eq!(failed.state, "interrupted");
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .starts_with("driver automatic resume failed:")
        );
        assert_eq!(reopened.driver(&accepted[0].id).unwrap().state, "running");
        reopened.stop_driver(&accepted[0].id).unwrap();
        reopened
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        drop(reopened);
        drop(storage);
        let reopened = self::service(root.path(), open_storage(&path));
        assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0, "{case}");
        assert_eq!(reopened.driver(&invalid.id).unwrap(), failed);
        assert!(reopened.tasks.status().is_idle());
        assert!(reopened.sessions.sessions().is_empty());
    }
}

#[tokio::test]
async fn automatic_restart_checkpoint_failure_cannot_schedule_or_change_proof() {
    for effect in ["ignore", "abort", "payload", "request"] {
        let root = fixture_directory();
        let path = root.path().join("metadata.sqlite");
        let storage = open_storage(&path);
        let (service, instance) = instance_service(root.path(), storage.clone()).await;
        let accepted = persist_automatic_restart_fixture(&service, &instance, 9).await;
        drop(service);
        drop(storage);
        let storage = open_storage(&path);
        let reopened = self::service(root.path(), storage.clone());
        let snapshot = || {
            storage.read(|db| {
                let mut query = db.prepare("SELECT driver_id,payload,request FROM benchmark_drivers ORDER BY driver_id")?;
                query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Option<Vec<u8>>>(2)?)))?
                    .collect::<Result<Vec<_>, _>>().map_err(BenchmarkError::from)
            }).unwrap()
        };
        let before = snapshot();
        let (when, body) = match effect {
            "ignore" => ("BEFORE", "SELECT RAISE(IGNORE)"),
            "abort" => ("BEFORE", "SELECT RAISE(ABORT,'checkpoint refused')"),
            "payload" => (
                "AFTER",
                "UPDATE benchmark_drivers SET payload=OLD.payload WHERE driver_id=NEW.driver_id",
            ),
            "request" => (
                "AFTER",
                "UPDATE benchmark_drivers SET request=NULL WHERE driver_id=NEW.driver_id",
            ),
            _ => unreachable!(),
        };
        storage.transaction(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER refuse_restart_checkpoint {when} UPDATE ON benchmark_drivers WHEN NEW.driver_id='{}' BEGIN {body}; END;", accepted[0].id))?;
            Ok::<_, BenchmarkError>(())
        }).unwrap();
        assert!(reopened.resume_interrupted_drivers().is_err(), "{effect}");
        assert_eq!(snapshot(), before, "{effect}");
        assert!(reopened.tasks.status().is_idle());
        assert!(reopened.sessions.sessions().is_empty());
        storage
            .read(|db| {
                assert_eq!(
                    db.query_row("SELECT count(*) FROM launch_intents", [], |row| row
                        .get::<_, usize>(0))?,
                    0
                );
                Ok::<_, BenchmarkError>(())
            })
            .unwrap();
    }
}

#[tokio::test]
async fn automatic_restart_global_failure_does_not_admit_later_drivers() {
    for case in ["malformed_suite", "busy", "task_owner"] {
        let root = fixture_directory();
        let path = root.path().join("metadata.sqlite");
        let storage = open_storage(&path);
        let (service, instance) = instance_service(root.path(), storage.clone()).await;
        let accepted = persist_automatic_restart_fixture(&service, &instance, 2).await;
        drop(service);
        drop(storage);
        let storage = open_storage(&path);
        let reopened = self::service(root.path(), storage.clone());
        if case == "malformed_suite" {
            storage
                .transaction(|tx| {
                    tx.execute(
                        "UPDATE benchmark_suites SET payload=?1 WHERE suite_id=?2",
                        params![b"{".as_slice(), accepted[1].suite_id],
                    )?;
                    Ok::<_, BenchmarkError>(())
                })
                .unwrap();
        } else if case == "task_owner" {
            reopened
                .tasks
                .shutdown(std::time::Duration::from_secs(2))
                .await
                .unwrap();
        }
        let _gate = (case == "busy").then(|| reopened.admit_suite(&accepted[1].suite_id).unwrap());
        let result = reopened.resume_interrupted_drivers();
        if matches!(case, "busy" | "task_owner") {
            assert!(matches!(result, Err(BenchmarkError::Busy)), "{case}");
        } else {
            assert!(matches!(result, Err(BenchmarkError::Unavailable)), "{case}");
        }
        let untouched = reopened.driver(&accepted[0].id).unwrap();
        assert_eq!(untouched.state, "interrupted");
        assert_eq!(
            untouched.error.as_deref(),
            Some("Driver interrupted by application restart")
        );
        assert!(reopened.tasks.status().is_idle());
        assert!(reopened.sessions.sessions().is_empty());
    }
}

#[tokio::test]
async fn observed_settlement_without_report_blocks_benchmarks_without_live_process_claims() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let (service, instance) = instance_service(root.path(), storage.clone()).await;
    let mut input: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
        "instance_id": instance,
        "suite_mode": "development",
    }))
    .unwrap();
    let mut suite = service.ensure_suite(&input).unwrap();
    input.suite_id = Some(suite.suite_id.clone());
    let mut request = service.launch_request(&input).unwrap();
    request.intent_key = suite.runs[0].launch_intent.clone();
    let scenario = benchmark_scenario(
        &suite.runs[0].profile,
        &suite.runs[0].run_type,
        &suite.mode,
        &suite.runs[0].benchmark_id,
    );
    let observed = service
        .launches
        .observe_unstarted_benchmark_for_test(request, scenario)
        .await;
    suite.runs[0].session_id = Some(observed.session_id.clone());
    suite.runs[0].launched_at = Some(observed.launched_at.clone());
    suite.runs[0].state = "running".into();
    service.save_suite(&suite).unwrap();
    let captured = service.suite(&suite.suite_id).unwrap().unwrap();
    assert!(matches!(
        service.tick(input.clone()).await,
        Err(BenchmarkError::Unavailable)
    ));
    assert_eq!(
        service.suite(&suite.suite_id).unwrap(),
        Some(captured.clone())
    );

    let mut driver = service.start_driver(input.clone()).await.unwrap();
    // Retain the driver's previously published process identity before its
    // next tick runs on this current-thread runtime.
    driver.active_session_id = Some(observed.session_id.clone());
    driver.last_session_id = Some(observed.session_id.clone());
    driver.last_run_index = Some(0);
    service.save_driver(&driver).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !service.tasks.status().is_idle() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let failed = service.driver(&driver.id).unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error, Some(BenchmarkError::Unavailable.to_string()));
    assert_eq!(failed.active_session_id, None);
    assert_eq!(
        failed.last_session_id.as_deref(),
        Some(observed.session_id.as_str())
    );
    assert_eq!(failed.pending_run_index, Some(1));
    assert_eq!(failed.launched_run_count, 1);
    assert!(service.reports.get(&observed.session_id).unwrap().is_none());
    assert!(service.sessions.sessions().is_empty());
    drop(service);
    drop(storage);

    let storage = open_storage(&path);
    let reopened = self::service(root.path(), storage.clone());
    assert!(matches!(
        reopened.tick(input.clone()).await,
        Err(BenchmarkError::Unavailable)
    ));
    assert_eq!(reopened.suite(&suite.suite_id).unwrap(), Some(captured));
    assert_eq!(reopened.driver(&driver.id).unwrap(), failed);
    assert!(matches!(
        reopened.launches.intent(suite.runs[0].launch_intent.as_deref().unwrap()).unwrap(),
        Some(LaunchIntentStatus::Accepted { session })
            if session.phase == SessionPhase::Exited
                && session.session_id == observed.session_id
                && session.instance_id == observed.instance_id
    ));
    assert!(
        reopened
            .reports
            .get(&observed.session_id)
            .unwrap()
            .is_none()
    );
    assert!(reopened.sessions.sessions().is_empty());
    assert!(reopened.tasks.status().is_idle());
    storage
        .read(|db| {
            let count: i64 =
                db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get(0))?;
            assert_eq!(count, 1);
            Ok::<_, StorageError>(())
        })
        .unwrap();
}
