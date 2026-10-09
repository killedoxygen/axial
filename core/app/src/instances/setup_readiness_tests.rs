use super::*;
use crate::{
    accounts::{
        credential_store::CredentialStore, directory::AccountDirectory, session::AuthService,
    },
    launch::{
        coordinator::{LaunchCoordinator, LaunchError},
        session::SessionManager,
    },
    network::{ClientConfig, ProviderClient},
    performance::PerformanceService,
    runtime::discovery::RuntimeDiscovery,
    skins::{ProfileMedia, library::SavedSkinLibrary, store::SavedSkinStore},
};

pub(super) struct LoaderCatalogFixture {
    pub builds: Vec<LoaderBuildRecord>,
    pub versions: Vec<axial_minecraft::LoaderGameVersion>,
    pub state: loaders::LoaderCatalogState,
}

fn fixture() -> (tempfile::TempDir, SetupService, Arc<AccountDirectory>) {
    let root = tempfile::Builder::new()
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap();
    let (service, accounts) = open_fixture(root.path(), crate::library::LibraryId::new());
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": 0, "performance_mode": "vanilla",
                "java_path_override": root.path().join("no-java-here").to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    seed_catalog(&service);
    (root, service, accounts)
}

fn open_fixture(
    root: &std::path::Path,
    library_id: crate::library::LibraryId,
) -> (SetupService, Arc<AccountDirectory>) {
    let library = match crate::library::LibraryLifecycle::open_with_id(root, library_id) {
        crate::library::LibraryOpenOutcome::Ready(library) => library,
        other => panic!("isolated test library did not open: {other:?}"),
    };
    compose_fixture(root, library)
}

fn compose_fixture(
    root: &std::path::Path,
    library: crate::library::LibraryLifecycle,
) -> (SetupService, Arc<AccountDirectory>) {
    let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
    storage
        .migrate(&[
            super::super::directory::MIGRATION,
            super::super::create::MIGRATION,
            super::super::delete::MIGRATION,
            crate::content::install::MIGRATION,
            crate::performance::mutation::MIGRATION,
            crate::accounts::directory::MIGRATION,
            crate::install::queue::MIGRATION,
            crate::performance::rules::MIGRATION,
            crate::skins::store::MIGRATION,
        ])
        .unwrap();
    let instances = Arc::new(InstanceService::new(
        super::super::directory::InstanceDirectories::new(
            super::super::directory::Registry::new(storage.clone()),
            library,
            crate::tasks::Exclusions::new(),
        ),
        crate::tasks::TaskOwner::new(16).unwrap(),
    ));
    let tasks = instances.tasks.clone();
    let directories = instances.directories().clone();
    let library = directories.library();
    let runtime = axial_minecraft::ManagedRuntimeCache::isolated_for_test().unwrap();
    let queue = Arc::new(
        InstallQueue::new(
            storage.clone(),
            library.clone(),
            directories.exclusions().clone(),
            tasks.clone(),
            runtime.clone(),
        )
        .unwrap(),
    );
    let client = ProviderClient::new(ClientConfig::default()).unwrap();
    let content = Arc::new(ContentService::new(client.clone()).unwrap());
    let settings = Arc::new(SettingsStore::new(storage.clone()).unwrap());
    let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
    let auth = Arc::new(AuthService::new(
        accounts.clone(),
        Arc::new(CredentialStore::isolated_for_tests()),
        tasks.clone(),
    ));
    let performance = PerformanceService::new(
        storage.clone(),
        directories.clone(),
        tasks.clone(),
        content,
        crate::performance::public_transfer_resolver(),
    )
    .unwrap();
    let root_pin = library.admit_application_root().unwrap();
    let skins = ProfileMedia::new(
        Arc::new(SavedSkinLibrary::new(
            SavedSkinStore::new(storage),
            root_pin.clone(),
        )),
        accounts.clone(),
        auth.clone(),
        tasks.clone(),
        root_pin,
    )
    .unwrap();
    let launch = Arc::new(LaunchCoordinator::new(
        directories,
        accounts.clone(),
        settings.clone(),
        queue.as_ref().clone(),
        RuntimeDiscovery::new(runtime, tasks.clone()),
        performance,
        auth,
        skins,
        SessionManager::new(tasks.clone()),
        tasks,
    ));
    let service = SetupService::new(
        instances,
        Arc::new(Catalog::new(client)),
        queue,
        settings,
        launch,
    );
    (service, accounts)
}

fn seed_catalog(service: &SetupService) {
    let pin = service.instances.directories().library().admit().unwrap();
    let operation = pin.managed_library().unwrap();
    let versions = ["1.21.4", "1.20.1"]
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "id":id, "type":"release", "url":format!("https://piston-meta.mojang.com/{id}.json"),
                "sha1":"a".repeat(40), "time":"2024-12-03T00:00:00Z", "releaseTime":"2024-12-03T00:00:00Z"
            })
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "latest": {"release":"1.21.4", "snapshot":"1.21.4"},
        "versions": versions
    });
    axial_minecraft::manifest::persist_version_manifest_cache_fixture_for_test(
        &operation,
        &serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn select_unowned_microsoft_account(accounts: &AccountDirectory) {
    use crate::accounts::{microsoft::MinecraftProfile, model::MicrosoftIdentity};

    let profile = uuid::Uuid::new_v4().simple().to_string();
    accounts
        .commit_microsoft(
            accounts.selection_revision().unwrap(),
            MicrosoftIdentity {
                login_id: uuid::Uuid::new_v4().to_string(),
                profile_id: profile.clone(),
                display_name: "UnownedPlayer".into(),
                credential_revision: 1,
                profile: MinecraftProfile {
                    id: profile,
                    name: "UnownedPlayer".into(),
                    skins: vec![],
                    capes: vec![],
                },
                owns_minecraft_java: false,
            },
        )
        .unwrap();
}

#[test]
fn installed_verification_pressure_does_not_trigger_install_fallback() {
    use axial_resource::{
        PhysicalIoClass, PhysicalWorkClass, PhysicalWorkRequest, process_physical_work,
    };
    use futures_util::FutureExt;
    use std::time::Duration;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if !runtime.block_on(crate::install::artifacts::tests::preparation_child(
        "instances::setup::readiness_tests::installed_verification_pressure_does_not_trigger_install_fallback",
    )) {
        return;
    }
    let (root, mut service, accounts) = runtime.block_on(async { fixture() });
    let catalog = stale_loader_catalog();
    let installed = catalog.builds[0].clone();
    service.loader_catalog_fixture = Some(catalog);
    let service = Arc::new(service);
    let library = service.instances.directories().library().clone();
    let library_id = library.snapshot().current.unwrap().library_id;
    let cache = service.installs.runtime_cache().clone();
    let work = process_physical_work();
    let pressure = Arc::new(Mutex::new(None));
    let clear_hooks = || {
        service
            .instances
            .registry()
            .storage()
            .transaction(|db| -> Result<(), StorageError> {
                db.update_hook(
                    None::<fn(crate::storage::rusqlite::hooks::Action, &str, &str, i64)>,
                );
                db.commit_hook(None::<fn() -> bool>);
                Ok(())
            })
    };
    let outcome = runtime.block_on(
        std::panic::AssertUnwindSafe(async {
            crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
            crate::install::queue::tests::install_ready_fixture(&service.installs, &installed.version_id)
                .await;
            let pin = library.admit().unwrap();
            let selection = "vanilla|1.21.4";
            let exact = format!(
                "loader_build|{}|{}",
                installed.component_id.as_str(),
                installed.build_id
            );
            let automatic = format!("loader_auto|{}|1.21.4", installed.component_id.as_str());
            let selections = [
                (selection, "1.21.4"),
                (exact.as_str(), installed.version_id.as_str()),
                (automatic.as_str(), installed.version_id.as_str()),
            ];
            for (selection, version) in selections {
                assert_eq!(
                    service.resolve(selection).await.unwrap().0.version_id,
                    version
                );
            }
            let queue_before = serde_json::to_value(service.installs.snapshot()).unwrap();
            let storage = service.instances.registry().storage();
            let read_inventory = || {
                storage.read(|db| -> Result<String, StorageError> {
                        Ok(db.query_row(
                            "SELECT inventory_json FROM installed_versions WHERE library_id=?1 AND version_id='1.21.4' AND state='ready'",
                            [library_id.to_string()], |row| row.get(0),
                        )?)
                    })
            };
            let write_inventory = |record: &str| {
                storage.transaction(|db| -> Result<_, StorageError> {
                        Ok(db.execute(
                            "UPDATE installed_versions SET inventory_json=?1 WHERE library_id=?2 AND version_id='1.21.4' AND state='ready'",
                            crate::storage::rusqlite::params![record, library_id.to_string()],
                        )?)
                    })
            };
            let encoded = read_inventory().unwrap();
            let mut padded = encoded.clone();
            padded.extend(std::iter::repeat_n(' ', (1 << 20) - padded.len()));
            assert_eq!(write_inventory(&padded).unwrap(), 1);
            let slots_pressure = work
                .try_reserve_scratch(work.scratch_limit_bytes() - (1 << 20))
                .unwrap();
            let before_decode = work.snapshot(PhysicalWorkClass::Foreground);
            let slots_ready = service
                .installs
                .ready_version(&pin, "1.21.4")
                .await
                .map(|_| ());
            let slots_resolution = service
                .resolve(selection)
                .await
                .map(|(target, _, _)| target.version_id);
            let after_decode = work.snapshot(PhysicalWorkClass::Foreground);
            drop(slots_pressure);
            let unchanged = read_inventory().unwrap();
            assert_eq!(write_inventory(&encoded).unwrap(), 1);
            service
                .installs
                .ready_version(&pin, "1.21.4")
                .await
                .unwrap()
                .revalidate()
                .unwrap();
            assert_eq!(
                service.resolve(selection).await.unwrap().0.version_id,
                "1.21.4"
            );
            assert_eq!(unchanged, padded);
            assert_eq!(
                serde_json::to_value(service.installs.snapshot()).unwrap(),
                queue_before
            );
            assert_eq!(before_decode, after_decode);
            assert_eq!(before_decode.available_scratch_bytes, 1 << 20);
            assert_eq!(before_decode.available_workers, 4);
            assert!(
                matches!(slots_ready, Err(InstallError::AtCapacity))
                    && matches!(slots_resolution, Err(InstanceError::Busy)),
                "decoded slots must be admitted without install fallback: ready={slots_ready:?}, resolution={slots_resolution:?}"
            );
            let mut refusals = Vec::new();
            for (selection, version) in selections {
                for worker_pressure in [false, true] {
                    let scratch =
                        (!worker_pressure).then(|| work.try_reserve_scratch(work.scratch_limit_bytes()));
                    let workers = if worker_pressure {
                        Some(
                            work.admit(PhysicalWorkRequest::foreground_parallel(
                                PhysicalIoClass::Read,
                                0,
                                4,
                            ))
                            .await,
                        )
                    } else {
                        None
                    };
                    let occupied = if worker_pressure {
                        matches!(&workers, Some(Ok(_)))
                    } else {
                        matches!(&scratch, Some(Ok(Some(_))))
                    };
                    let ready = service
                        .installs
                        .ready_version(&pin, version)
                        .await
                        .map(|_| ());
                    let mut resolution = Box::pin(service.resolve(selection));
                    let observed = tokio::time::timeout(Duration::from_secs(2), &mut resolution).await;
                    let prompt = observed.is_ok();
                    drop((scratch, workers));
                    let result = match observed {
                        Ok(result) => result,
                        Err(_) => resolution.await,
                    };
                    refusals.push((
                        selection,
                        occupied,
                        prompt,
                        ready,
                        result.map(|(target, _, _)| target.version_id),
                    ));
                    let restored = service.installs.ready_version(&pin, version).await.unwrap();
                    assert_eq!(restored.version().id, version);
                    restored.revalidate().unwrap();
                    drop(restored);
                    assert_eq!(
                        service.resolve(selection).await.unwrap().0.version_id,
                        version
                    );
                }
            }
            assert!(service.instances.registry().list().unwrap().is_empty());
            assert!(service.instances.pending().unwrap().is_empty());
            assert_eq!(
                serde_json::to_value(service.installs.snapshot()).unwrap(),
                queue_before
            );
            assert!(
                refusals
                    .iter()
                    .all(|(_, occupied, prompt, ready, result)| *occupied
                        && *prompt
                        && matches!(ready, Err(InstallError::AtCapacity))
                        && matches!(result, Err(InstanceError::Busy))),
                "verification pressure must remain retryable without install fallback: {refusals:?}"
            );
            use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
            use crate::storage::rusqlite::hooks::Action;
            for worker_pressure in [false, true] {
                let row = Arc::new(AtomicI64::new(0));
                let armed = Arc::new(AtomicBool::new(false));
                let canary = root.path().join("creation-canary.txt");
                std::fs::write(&canary, b"preserve publication under pressure").unwrap();
                service
                    .instances
                    .registry()
                    .storage()
                    .transaction(|db| -> Result<(), StorageError> {
                        db.update_hook(Some({
                            let row = row.clone();
                            let armed = armed.clone();
                            move |action, database: &str, table: &str, id| {
                                if database == "main" && table == "instances" {
                                    match action {
                                        Action::SQLITE_INSERT => {
                                            row.store(id, Ordering::SeqCst);
                                        }
                                        Action::SQLITE_UPDATE if row.load(Ordering::SeqCst) == id => {
                                            armed.store(true, Ordering::SeqCst);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }));
                        db.commit_hook(Some({
                            let pressure = pressure.clone();
                            let work = work.clone();
                            move || {
                                if armed.swap(false, Ordering::SeqCst) {
                                    let scratch = work.try_reserve_scratch(if worker_pressure {
                                        0
                                    } else {
                                        work.scratch_limit_bytes()
                                    });
                                    let workers = worker_pressure
                                        .then(|| {
                                            work.admit(PhysicalWorkRequest::foreground_parallel(
                                                PhysicalIoClass::Read,
                                                0,
                                                4,
                                            ))
                                            .now_or_never()
                                        })
                                        .flatten();
                                    *pressure.lock().unwrap() = Some((scratch, workers));
                                }
                                false
                            }
                        }));
                        Ok(())
                    })
                    .unwrap();
                let name = if worker_pressure {
                    "PublishedUnderWorkerPressure"
                } else {
                    "PublishedUnderScratchPressure"
                };
                let request = serde_json::from_value(serde_json::json!({
                    "name": name, "selection_id": selection,
                }))
                .unwrap();
                let mut creation = Box::pin(service.create(request));
                let observed = tokio::time::timeout(Duration::from_secs(2), &mut creation).await;
                let prompt = observed.is_ok();
                clear_hooks().unwrap();
                let held = work.snapshot(PhysicalWorkClass::Foreground);
                let occupied = pressure.lock().unwrap().take();
                let full = if worker_pressure {
                    matches!(&occupied, Some((Ok(None), Some(Ok(_)))))
                        && held.available_workers == 0
                        && held.available_scratch_bytes == work.scratch_limit_bytes()
                } else {
                    matches!(&occupied, Some((Ok(Some(_)), None)))
                        && held.available_workers == 4
                        && held.available_scratch_bytes == 0
                };
                drop(occupied);
                let created = match observed {
                    Ok(result) => result,
                    Err(_) => creation.await,
                }
                .unwrap();
                let record = service
                    .instances
                    .registry()
                    .get_live(&created.instance.instance.id)
                    .unwrap();
                assert!(
                    prompt && full && row.load(Ordering::SeqCst) != 0,
                    "postcommit pressure: workers={worker_pressure}, held={held:?}"
                );
                assert_eq!(created.instance.instance.name, name);
                assert_eq!(
                    created.instance.instance,
                    super::super::create::public_instance(record.instance.clone())
                );
                let response = serde_json::to_value(created).unwrap();
                assert_eq!(
                    response["view_model"],
                    serde_json::json!({
                        "state_id": "created_install_unavailable", "tone": "warn", "title": "Instance created",
                        "summary": "Instance created. Installation could not be queued.",
                        "detail": "Use Install on this instance to try again.",
                    })
                );
                assert!(response.get("install_queue").is_none());
                assert!(service.instances.pending().unwrap().is_empty());
                let admitted = service
                    .instances
                    .directories()
                    .admit(&record.instance.id)
                    .unwrap();
                admitted.validate_current().unwrap();
                for directory in super::super::create::INITIAL_DIRECTORIES {
                    assert!(
                        admitted
                            .game_directory()
                            .read_projection()
                            .unwrap()
                            .join(directory)
                            .is_dir()
                    );
                }
                drop(admitted);
                assert_eq!(
                    serde_json::to_value(service.installs.snapshot()).unwrap(),
                    queue_before
                );
                assert_eq!(
                    std::fs::read(&canary).unwrap(),
                    b"preserve publication under pressure"
                );
                service
                    .installs
                    .ready_version(&pin, "1.21.4")
                    .await
                    .unwrap()
                    .revalidate()
                    .unwrap();
            }
            accounts.create_offline_account("PressurePlayer").unwrap();
            let instance =
                super::super::create::tests::create(&service.instances, "Pressure").await;
            let request = serde_json::from_value(serde_json::json!({
                "instance_id": instance.id,
                "intent_key": uuid::Uuid::new_v4().to_string(),
            }))
            .unwrap();
            let workers = work
                .admit(PhysicalWorkRequest::foreground_parallel(
                    PhysicalIoClass::Read,
                    0,
                    4,
                ))
                .await
                .unwrap();
            let mut launch = Box::pin(service.launch.launch(request));
            let observed = tokio::time::timeout(Duration::from_secs(2), &mut launch).await;
            let prompt = observed.is_ok();
            drop(workers);
            let result = match observed {
                Ok(result) => result,
                Err(_) => launch.await,
            };
            let restored = service.launch.preflight(instance.id).await;
            assert!(matches!(
                restored.error.map(|error| error.code),
                Some(LaunchError::RuntimeFailure(_)) | Some(LaunchError::RuntimeUnavailable)
            ));
            assert!(
                prompt && matches!(result, Err(LaunchError::AtCapacity)),
                "Play must preserve verification capacity refusal: {result:?}"
            );

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let client = ProviderClient::new(ClientConfig::default()).unwrap();
            let content = Arc::new(ContentService::with_base_url(
                client.clone(), &origin,
                crate::network::OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
            ).unwrap());
            let mutations = Arc::new(ContentMutations::new(
                service.instances.directories().clone(), client, service.instances.tasks.clone(),
            ));
            let queued_work = super::tests::pending_content_work(
                service.instances.clone(), content.clone(), mutations.clone(),
            ).await;
            let queue = service.installs.as_ref().clone().with_content(content, mutations);
            let record = queued_work.instance().record().clone();
            let directory = queued_work.instance().game_directory().read_projection().unwrap();
            let StoredSetupIntent::Content(stored) = &queued_work.stored else { unreachable!() };
            let mut manifest = crate::content::provenance::ContentManifest::default();
            let mut paths = Vec::new();
            for artifact in stored.artifacts.as_ref().unwrap() {
                manifest.try_upsert(crate::content::provenance::ManifestEntry::managed(
                    artifact.canonical_id.clone(), artifact.provider, artifact.project_id.clone(),
                    artifact.version_id.clone(), artifact.kind, &artifact.file,
                    artifact.dependencies.clone(), artifact.title.clone(),
                ).unwrap()).unwrap();
                let path = directory.join("resourcepacks").join(&artifact.file.filename);
                std::fs::write(&path, b"fixture").unwrap();
                paths.push(path);
            }
            let manifest_path = directory.join(crate::content::provenance::MANIFEST_FILE);
            std::fs::write(&manifest_path, manifest.encode_managed().unwrap()).unwrap();
            paths.push(manifest_path);
            let canary = directory.join("notes.txt");
            std::fs::write(&canary, b"preserve accepted setup files").unwrap();
            paths.push(canary);
            let contents = paths.iter().map(|path| std::fs::read(path).unwrap()).collect::<Vec<_>>();
            let blocker = service.instances.directories().exclusions().try_acquire(
                std::iter::empty::<String>(),
                [crate::install::queue::library_artifact(&pin.library_id().to_string())],
            ).unwrap();
            let accepted = queue.enqueue_setup_content(
                queued_work.request(), queued_work.prerequisite(), queued_work.clone(),
            ).await.unwrap();
            let started = accepted.started_install.unwrap();
            assert_eq!(accepted.items.len(), 1);
            assert_eq!(accepted.items[0].kind, "content");
            assert!(accepted.active.is_none());
            let (_, mut updates) = queue.subscribe();
            let workers = work.admit(PhysicalWorkRequest::foreground_parallel(
                PhysicalIoClass::Read, 0, 4,
            )).await.unwrap();
            drop(blocker);
            queue.resume_queued();
            let refused = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let status = queue.status(&started.install_id).unwrap();
                    if status.done { break status; }
                    updates.changed().await.unwrap();
                }
            }).await;
            let pending_after_refusal = super::has_pending(
                service.instances.registry().storage(), &record.instance.id,
            ).unwrap();
            let preserved_after_refusal = paths.iter().map(|path| std::fs::read(path).unwrap()).collect::<Vec<_>>();
            let mut before_retry = serde_json::to_value(queue.snapshot()).unwrap();
            let pressured_retry = queue.retry_setup_content(
                queued_work.request(), queued_work.prerequisite(), queued_work.clone(),
            ).await;
            let mut after_retry = serde_json::to_value(queue.snapshot()).unwrap();
            for snapshot in [&mut before_retry, &mut after_retry] {
                let fields = snapshot.as_object_mut().unwrap();
                fields.remove("revision");
                fields.remove("registry_revision");
            }
            let retry_unchanged = after_retry == before_retry;
            drop(workers);
            let refused = refused.expect("accepted setup must refuse worker pressure promptly");
            let retried = queue.retry_setup_content(
                queued_work.request(), queued_work.prerequisite(), queued_work.clone(),
            ).await.unwrap().started_install.unwrap();
            let restored = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let status = queue.status(&retried.install_id).unwrap();
                    if status.done { break status; }
                    updates.changed().await.unwrap();
                }
            }).await.unwrap();
            assert_eq!(refused.operation_id, started.operation_id);
            assert_eq!(restored.operation_id, retried.operation_id);
            assert_ne!(retried.install_id, started.install_id);
            assert_ne!(retried.operation_id, started.operation_id);
            assert_eq!(queue.status(&started.install_id).unwrap(), refused);
            assert_eq!(refused.outcome, Some(crate::install::model::InstallOutcome::Failed));
            assert!(pending_after_refusal);
            assert_eq!(preserved_after_refusal, contents);
            assert!(matches!(pressured_retry, Err(InstallError::AtCapacity)) && retry_unchanged);
            assert_eq!(restored.outcome, Some(crate::install::model::InstallOutcome::Succeeded));
            assert!(!super::has_pending(service.instances.registry().storage(), &record.instance.id).unwrap());
            assert_eq!(service.instances.registry().get_live(&record.instance.id).unwrap(), record);
            assert_eq!(paths.iter().map(|path| std::fs::read(path).unwrap()).collect::<Vec<_>>(), contents);
            assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
            assert_eq!(refused.failure_view_model.unwrap().summary,
                "The install queue is full. Wait for an installation to finish.");
            service.instances.registry().list().unwrap()
        })
        .catch_unwind(),
    );
    let hooks_cleared = clear_hooks().is_ok();
    drop(pressure.lock().unwrap().take());
    service.installs.close_admission();
    let shutdown = runtime.block_on(service.instances.tasks.shutdown(Duration::from_secs(3)));
    let observers = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), service.installs.join_observers()).await
    });
    let queue_settled = service.installs.shutdown_queued();
    let state = work.snapshot(PhysicalWorkClass::Foreground);
    let quiescent = hooks_cleared
        && shutdown.is_ok()
        && matches!(observers, Ok(Ok(())))
        && queue_settled.is_ok()
        && state.available_workers == 4
        && state.running_workers == 0
        && state.active_admissions == 0
        && state.available_scratch_bytes == work.scratch_limit_bytes();
    if !quiescent {
        let retained = root.keep();
        std::mem::forget((service, accounts, library, cache, runtime));
        eprintln!(
            "selection pressure fixture retained: {}",
            retained.display()
        );
        panic!("selection pressure owners did not settle");
    }
    drop((service, accounts));
    let settled = cache.settle().is_ok() && library.try_preserve().is_ok();
    if !settled {
        let retained = root.keep();
        std::mem::forget((library, cache, runtime));
        eprintln!(
            "selection pressure fixture retained: {}",
            retained.display()
        );
        panic!("selection pressure roots did not settle");
    }
    let records = match outcome {
        Ok(records) => records,
        Err(panic) => {
            let retained = root.keep();
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "selection pressure fixture retained: {}",
                retained.display()
            );
            std::panic::resume_unwind(panic);
        }
    };
    drop((library, cache));
    let library = match crate::library::LibraryLifecycle::open_with_id(root.path(), library_id) {
        crate::library::LibraryOpenOutcome::Ready(library) => library,
        refused => {
            let retained = root.keep();
            std::mem::forget((refused, runtime));
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "pressure reopen refused; retained: {}",
                retained.display()
            );
            panic!("pressure library did not reopen");
        }
    };
    let reopened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async { compose_fixture(root.path(), library.clone()) })
    }));
    let (reopened, accounts) = match reopened {
        Ok(owners) => owners,
        Err(panic) => {
            let retained = root.keep();
            std::mem::forget((library, runtime));
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "pressure composition refused; retained: {}",
                retained.display()
            );
            std::panic::resume_unwind(panic);
        }
    };
    let cache = reopened.installs.runtime_cache().clone();
    let outcome = runtime.block_on(
        std::panic::AssertUnwindSafe(async {
            assert_eq!(reopened.instances.registry().list().unwrap(), records);
            assert!(reopened.instances.pending().unwrap().is_empty());
            let public = reopened.list().await.unwrap();
            assert_eq!(
                public
                    .instances
                    .iter()
                    .map(|instance| instance.instance.clone())
                    .collect::<Vec<_>>(),
                records
                    .iter()
                    .map(|record| super::super::create::public_instance(record.instance.clone()))
                    .collect::<Vec<_>>()
            );
            let queue = reopened.installs.snapshot();
            assert!(
                queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none()
            );
            let published = records
                .iter()
                .filter(|record| {
                    matches!(
                        record.instance.name.as_str(),
                        "PublishedUnderScratchPressure" | "PublishedUnderWorkerPressure"
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(published.len(), 2);
            for record in published {
                let admitted = reopened
                    .instances
                    .directories()
                    .admit(&record.instance.id)
                    .unwrap();
                admitted.validate_current().unwrap();
                let directory = admitted.game_directory().read_projection().unwrap();
                for name in super::super::create::INITIAL_DIRECTORIES {
                    assert!(
                        std::fs::read_dir(directory.join(name))
                            .unwrap()
                            .next()
                            .is_none()
                    );
                }
            }
            assert_eq!(
                std::fs::read(root.path().join("creation-canary.txt")).unwrap(),
                b"preserve publication under pressure"
            );
            let pin = library.admit().unwrap();
            for version in ["1.21.4", installed.version_id.as_str()] {
                reopened
                    .installs
                    .ready_version(&pin, version)
                    .await
                    .unwrap()
                    .revalidate()
                    .unwrap();
            }
        })
        .catch_unwind(),
    );
    reopened.installs.close_admission();
    let shutdown = runtime.block_on(reopened.instances.tasks.shutdown(Duration::from_secs(3)));
    let observers = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), reopened.installs.join_observers()).await
    });
    let queue_settled = reopened.installs.shutdown_queued();
    if shutdown.is_err() || !matches!(observers, Ok(Ok(()))) || queue_settled.is_err() {
        let retained = root.keep();
        std::mem::forget((reopened, accounts, library, cache, runtime));
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "reopened pressure owners retained: {}",
            retained.display()
        );
        std::panic::resume_unwind(
            outcome
                .err()
                .unwrap_or_else(|| Box::new("reopened owners did not settle")),
        );
    }
    drop((reopened, accounts));
    let runtime_settled = cache.settle();
    let preserved = library.try_preserve();
    if runtime_settled.is_err() || preserved.is_err() || outcome.is_err() {
        let retained = root.keep();
        if runtime_settled.is_err() || preserved.is_err() {
            std::mem::forget((library, cache, runtime));
        }
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "reopened pressure fixture retained: {}",
            retained.display()
        );
        std::panic::resume_unwind(
            outcome
                .err()
                .unwrap_or_else(|| Box::new("reopened roots did not settle")),
        );
    }
}

fn profile_build(
    component: LoaderComponentId,
    minecraft: &str,
    loader: &str,
    reason: LoaderSelectionReason,
) -> LoaderBuildRecord {
    let (strategy, base) = match component {
        LoaderComponentId::Fabric => ("fabric_profile", "https://meta.fabricmc.net/v2/versions"),
        LoaderComponentId::Quilt => ("quilt_profile", "https://meta.quiltmc.org/v3/versions"),
        _ => panic!("profile fixture requires Fabric or Quilt"),
    };
    serde_json::from_value(serde_json::json!({
        "component_id":component, "component_name":component.display_name(),
        "build_id":loaders::build_id_for(component, minecraft, loader),
        "minecraft_version":minecraft, "loader_version":loader,
        "version_id":loaders::installed_version_id_for(component, minecraft, loader).unwrap(),
        "build_meta":{"selection":{"reason":reason}},
        "strategy":strategy, "artifact_kind":"profile_json", "installability":"installable",
        "install_source":{"kind":"profile_json", "url":format!("{base}/loader/{minecraft}/{loader}/profile/json")}
    }))
    .unwrap()
}

fn stale_loader_catalog() -> LoaderCatalogFixture {
    LoaderCatalogFixture {
        builds: vec![profile_build(
            LoaderComponentId::Fabric,
            "1.21.4",
            "0.16.14",
            LoaderSelectionReason::Stable,
        )],
        versions: vec![serde_json::from_value(serde_json::json!({"id":"1.21.4"})).unwrap()],
        state: loaders::LoaderCatalogState {
            availability: loaders::LoaderAvailability {
                fresh: false,
                stale: true,
                cache_hit: true,
                ..Default::default()
            },
        },
    }
}

#[tokio::test]
async fn loader_picker_retains_beta_labels_without_treating_unknown_as_an_unstable_default() {
    let (_root, mut service, _) = fixture();
    let cases = [
        (LoaderSelectionReason::Recommended, false),
        (LoaderSelectionReason::LatestStable, false),
        (LoaderSelectionReason::Stable, false),
        (LoaderSelectionReason::Unlabeled, false),
        (LoaderSelectionReason::Latest, true),
        (LoaderSelectionReason::LatestUnstable, true),
        (LoaderSelectionReason::Unstable, true),
        (LoaderSelectionReason::Unknown, false),
    ];
    let mut catalog = stale_loader_catalog();
    catalog.state.availability.fresh = true;
    catalog.state.availability.stale = false;
    catalog.builds = cases
        .iter()
        .enumerate()
        .map(|(index, (reason, _))| {
            profile_build(
                LoaderComponentId::Fabric,
                "1.21.4",
                &format!("0.16.{}", index + 14),
                *reason,
            )
        })
        .collect();
    let unknown = catalog.builds.last().unwrap().clone();
    service.loader_catalog_fixture = Some(catalog);
    let mut service = Arc::new(service);
    let view = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(view.auto.enabled);
    for (index, (option, (_, beta))) in view.builds.iter().zip(cases).enumerate() {
        assert_eq!(option.channel_id, if beta { "beta" } else { "stable" });
        assert_eq!(option.channel_label, if beta { "Beta" } else { "Stable" });
        assert_eq!(option.recommended, index == 0);
        assert!(option.enabled);
    }
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds = vec![unknown];
    let unknown_only = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!unknown_only.auto.enabled);
    assert!(!unknown_only.builds[0].recommended);
    assert_eq!(unknown_only.builds[0].channel_id, "stable");
    assert!(unknown_only.builds[0].enabled);
}

#[tokio::test]
async fn incompatible_quilt_build_is_disabled_and_rejected_even_when_installed() {
    let (_root, mut service, _) = fixture();
    let component = LoaderComponentId::Quilt;
    let minecraft = "26.1.2";
    let incompatible = profile_build(
        component,
        minecraft,
        "0.29.2",
        LoaderSelectionReason::Unlabeled,
    );
    let compatible = profile_build(
        component,
        minecraft,
        "0.30.0-beta.8",
        LoaderSelectionReason::Unstable,
    );
    let mut catalog = stale_loader_catalog();
    catalog.builds = vec![incompatible.clone(), compatible.clone()];
    catalog.state.availability.fresh = true;
    catalog.state.availability.stale = false;
    service.loader_catalog_fixture = Some(catalog);
    for version in [minecraft, &incompatible.version_id, &compatible.version_id] {
        crate::install::queue::tests::install_ready_fixture(&service.installs, version).await;
    }
    let pin = service.instances.directories().library().admit().unwrap();
    assert!(
        service
            .installs
            .ready_version(&pin, &incompatible.version_id)
            .await
            .is_ok(),
        "the compatibility gate must not rely on missing or damaged artifacts"
    );
    let mut service = Arc::new(service);
    for fresh in [true, false] {
        let availability = &mut Arc::get_mut(&mut service)
            .unwrap()
            .loader_catalog_fixture
            .as_mut()
            .unwrap()
            .state
            .availability;
        availability.fresh = fresh;
        availability.stale = !fresh;
        let view = service
            .loader_builds(component.as_str(), minecraft)
            .await
            .unwrap();
        assert!(view.auto.enabled);
        assert!(view.builds[0].installed && !view.builds[0].enabled);
        assert!(!view.builds[0].recommended);
        assert_eq!(
            view.builds[0].disabled_reason.as_deref(),
            Some("This Quilt build is known to be incompatible with Minecraft 26.1.2.")
        );
        assert!(view.builds[1].installed && view.builds[1].enabled && view.builds[1].recommended);
        assert_eq!(view.builds[1].channel_label, "Beta");
    }
    let automatic = format!("loader_auto|{}|{minecraft}", component.as_str());
    assert_eq!(
        service.resolve(&automatic).await.unwrap().0.version_id(),
        compatible.version_id
    );
    let exact_compatible = format!(
        "loader_build|{}|{}",
        component.as_str(),
        compatible.build_id
    );
    assert_eq!(
        service
            .resolve(&exact_compatible)
            .await
            .unwrap()
            .0
            .version_id(),
        compatible.version_id
    );
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds = vec![incompatible.clone()];
    let unavailable = service
        .loader_builds(component.as_str(), minecraft)
        .await
        .unwrap();
    assert!(!unavailable.auto.enabled && !unavailable.builds[0].enabled);
    assert!(matches!(
        service.resolve(&automatic).await,
        Err(InstanceError::VersionUnavailable)
    ));
    let before = service.installs.snapshot();
    let result = service
        .create(CreateInstanceRequest {
            name: "Incompatible installed Quilt".into(),
            selection_id: format!(
                "loader_build|{}|{}",
                component.as_str(),
                incompatible.build_id
            ),
            ..Default::default()
        })
        .await;
    assert!(matches!(result, Err(InstanceError::VersionUnavailable)));
    assert!(service.instances.registry().list().unwrap().is_empty());
    assert_eq!(service.installs.snapshot(), before);
}

#[tokio::test]
async fn loader_picker_refuses_a_degraded_library_and_recovers_after_restoration() {
    use futures_util::FutureExt;

    let (root, mut service, _) = fixture();
    let catalog = stale_loader_catalog();
    let build = catalog.builds[0].clone();
    service.loader_catalog_fixture = Some(catalog);
    let service = Arc::new(service);
    let journey = std::panic::AssertUnwindSafe(async {
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        crate::install::queue::tests::install_ready_fixture(&service.installs, &build.version_id)
            .await;
        let pin = service.instances.directories().library().admit().unwrap();
        let receipt = service
            .installs
            .ready_version(&pin, &build.version_id)
            .await
            .unwrap();
        let library = pin.read_projection().unwrap();
        let protected: Vec<_> = ["1.21.4", build.version_id.as_str()]
            .into_iter()
            .flat_map(|id| {
                let library = &library;
                ["json", "jar"].map(move |extension| {
                    let path = library.join(format!("versions/{id}/{id}.{extension}"));
                    let bytes = std::fs::read(&path).unwrap();
                    (path, bytes)
                })
            })
            .collect();
        let healthy = service
            .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
            .await
            .unwrap();
        let queue_before = service.installs.snapshot();
        let instances_before = service.instances.registry().list().unwrap();
        let pending_before = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let external = library.join("versions/external-degraded-entry");
        assert!(
            std::fs::symlink_metadata(&external)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        );
        std::fs::create_dir(&external).unwrap();
        let metadata = external.join("external-degraded-entry.json");
        let malformed = b"{not valid external version metadata\n";
        std::fs::write(&metadata, malformed).unwrap();
        let degraded = service
            .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
            .await
            .err();
        let preserved = std::fs::read(&metadata).unwrap();
        let queue_after = service.installs.snapshot();
        let instances_after = service.instances.registry().list().unwrap();
        let pending_after = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        receipt.revalidate().unwrap();
        std::fs::remove_file(&metadata).unwrap();
        std::fs::remove_dir(&external).unwrap();
        let restored = service
            .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
            .await
            .unwrap();
        drop(receipt);
        drop(pin);
        move || {
            assert_eq!(preserved, malformed);
            assert!(
                std::fs::symlink_metadata(external)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
            for (path, bytes) in protected {
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            assert_eq!(queue_after, queue_before);
            assert_eq!(instances_after, instances_before);
            assert_eq!(pending_after, pending_before);
            for view in [&healthy, &restored] {
                assert!(view.auto.enabled);
                assert_eq!(view.builds.len(), 1);
                assert_eq!(view.builds[0].build_id, build.build_id);
                assert!(view.builds[0].installed && view.builds[0].enabled);
            }
            assert!(
                matches!(degraded, Some(InstanceError::InstalledVersionsDegraded)),
                "{degraded:?}"
            );
        }
    })
    .catch_unwind()
    .await;
    let shutdown = std::panic::AssertUnwindSafe(
        service
            .instances
            .tasks
            .shutdown(std::time::Duration::from_secs(5)),
    )
    .catch_unwind()
    .await;
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match shutdown {
            Ok(result) => assert!(result.is_ok(), "{result:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
        assert!(service.instances.tasks.status().is_idle());
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!("Retained loader picker fixture: {}", root.keep().display());
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn instance_enrichment_leaves_resource_counts_to_detailed_reads_even_during_launch() {
    let (_root, service, _) = fixture();
    let instance = super::super::create::tests::create(&service.instances, "Resource counts").await;
    let game = super::super::create::tests::payload_path(&service.instances, &instance.id);
    std::fs::write(game.join("mods/.axial-lock.json"), b"internal metadata").unwrap();
    std::fs::create_dir(game.join("mods/.axial-performance")).unwrap();
    std::fs::write(game.join("mods/active.jar"), b"mod").unwrap();
    std::fs::write(game.join("mods/inactive.jar.disabled"), b"disabled mod").unwrap();
    std::fs::create_dir(game.join("saves/World")).unwrap();
    std::fs::write(game.join("resourcepacks/pack.zip"), b"resources").unwrap();
    std::fs::write(game.join("shaderpacks/pack.zip"), b"shaders").unwrap();
    for during_launch in [false, true] {
        let launch =
            during_launch.then(|| service.instances.directories().admit(&instance.id).unwrap());
        let enriched = service.enrich(instance.clone(), &[]).await;
        assert_eq!(
            [
                enriched.saves_count,
                enriched.mods_count,
                enriched.resource_count,
                enriched.shader_count,
            ],
            [0; 4]
        );
        assert!(!enriched.counts_available);
        let read = service
            .instances
            .directories()
            .admit_read(&instance.id)
            .unwrap();
        let mods = crate::resources::mods::list_mods(read.game_directory()).unwrap();
        read.validate_current().unwrap();
        assert_eq!(mods.len(), 2);
        assert_eq!(mods.iter().filter(|entry| entry.enabled).count(), 1);
        drop(launch);
    }
}

#[tokio::test]
async fn resolution_rejects_library_drift_during_a_catalog_response() {
    use futures_util::FutureExt;
    use std::time::Duration;

    for drift in [false, true] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (mut service, _) = open_fixture(root.path(), crate::library::LibraryId::new());
        let manifest = crate::catalog::tests::manifest(&[("1.21.11", "release")]);
        let (requested, observed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (catalog, server) =
            crate::catalog::tests::fixture_catalog(manifest.clone(), Some((requested, released)));
        service.catalog = Arc::new(catalog);
        let service = Arc::new(service);
        let mut release = Some(release);
        let mut waiter = None;
        let journey = std::panic::AssertUnwindSafe(async {
            let pin = service.instances.directories().library().admit().unwrap();
            let operation = pin.managed_library().unwrap();
            operation.prepare_layout().unwrap();
            let versions = pin
                .directory()
                .unwrap()
                .open_directory(&axial_fs::LeafName::new("versions").unwrap())
                .unwrap();
            let revision_before = versions.revision().unwrap();
            let canary = root.path().join("unrelated-user-file.txt");
            std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
            let absent = |path: &std::path::Path| {
                std::fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            };
            let target = root.path().join("versions/1.21.11");
            let instance_parent = root.path().join("instances");
            let cache_absent = absent(&root.path().join("cache/version_manifest_v2.json"));
            let target_absent = absent(&target);
            let namespace_absent = absent(&instance_parent);
            let instances_before = service.instances.registry().list().unwrap();
            let pending_before =
                serde_json::to_value(service.instances.pending().unwrap()).unwrap();
            let queue_before = service.installs.snapshot();
            waiter = Some(tokio::spawn({
                let service = service.clone();
                async move { service.resolve("vanilla|1.21.11").await }
            }));
            tokio::time::timeout(Duration::from_secs(5), observed)
                .await
                .expect("catalog request was not observed")
                .expect("catalog request observer closed");
            let pending_response = !waiter.as_ref().unwrap().is_finished();
            let external = root.path().join("versions/external-degraded-entry");
            let external_absent = absent(&external);
            let metadata = external.join("external-degraded-entry.json");
            let malformed = b"{not valid external version metadata\n";
            let changed_revision = if drift {
                std::fs::create_dir(&external).unwrap();
                std::fs::write(&metadata, malformed).unwrap();
                Some(
                    tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let revision = versions.revision().unwrap();
                            if revision != revision_before {
                                break revision;
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await,
                )
            } else {
                None
            };
            release.take().unwrap().send(()).unwrap();
            let old_result = tokio::time::timeout(Duration::from_secs(5), waiter.as_mut().unwrap())
                .await
                .expect("original resolution did not settle")
                .unwrap()
                .map(|(target, request, admission)| {
                    let valid = admission.validate();
                    (target.version_id().to_owned(), request, valid)
                });
            drop(waiter.take());
            // Successful cache publication proves a complete, validated HTTP body,
            // independently of the VersionUnavailable result under test.
            let cached = axial_minecraft::manifest::read_cached_manifest_bytes(&operation).unwrap();
            let preserved = if drift {
                let bytes = std::fs::read(&metadata).unwrap();
                std::fs::remove_file(&metadata).unwrap();
                std::fs::remove_dir(&external).unwrap();
                Some(bytes)
            } else {
                None
            };
            waiter = Some(tokio::spawn({
                let service = service.clone();
                async move { service.resolve("vanilla|1.21.11").await }
            }));
            let fresh_result =
                tokio::time::timeout(Duration::from_secs(5), waiter.as_mut().unwrap())
                    .await
                    .expect("fresh resolution did not settle")
                    .unwrap()
                    .map(|(target, request, admission)| {
                        let valid = admission.validate();
                        (target.version_id().to_owned(), request, valid)
                    });
            drop(waiter.take());
            let instances_after = service.instances.registry().list().unwrap();
            let pending_after = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
            let queue_after = service.installs.snapshot();
            let target_still_absent = absent(&target);
            let namespace_still_absent = absent(&instance_parent);
            let external_removed = absent(&external);
            drop(versions);
            drop(operation);
            drop(pin);
            move || {
                assert!(cache_absent && target_absent && namespace_absent && external_absent);
                assert!(
                    pending_response,
                    "HTTP body did not hold resolution pending"
                );
                let expected_request = InstallQueueRequest::Vanilla {
                    version_id: "1.21.11".into(),
                };
                if drift {
                    assert_ne!(changed_revision.unwrap().unwrap(), revision_before);
                    assert!(matches!(old_result, Err(InstanceError::VersionUnavailable)));
                    assert_eq!(preserved.as_deref(), Some(malformed.as_slice()));
                } else {
                    assert!(changed_revision.is_none() && preserved.is_none());
                    let (version, request, valid) = old_result.unwrap();
                    assert_eq!(version, "1.21.11");
                    assert_eq!(request, expected_request);
                    assert!(
                        valid.is_ok(),
                        "the undisturbed original admission must remain valid"
                    );
                }
                assert_eq!(cached, (manifest, true));
                assert!(external_removed && target_still_absent && namespace_still_absent);
                let (version, request, valid) = fresh_result.unwrap();
                assert_eq!(version, "1.21.11");
                assert_eq!(request, expected_request);
                assert!(
                    valid.is_ok(),
                    "fresh resolution did not return valid admission"
                );
                assert!(instances_before.is_empty());
                assert_eq!(instances_after, instances_before);
                assert_eq!(pending_before, serde_json::json!([]));
                assert_eq!(pending_after, pending_before);
                assert_eq!(queue_after, queue_before);
                assert_eq!(
                    std::fs::read(canary).unwrap(),
                    b"preserve unrelated user bytes\n"
                );
            }
        })
        .catch_unwind()
        .await;
        if let Some(release) = release.take() {
            let _ = release.send(());
        }
        service.installs.close_admission();
        let shutdown =
            std::panic::AssertUnwindSafe(service.instances.tasks.shutdown(Duration::from_secs(5)))
                .catch_unwind()
                .await;
        let remaining = match waiter.take() {
            Some(mut waiter) => {
                let result = tokio::time::timeout(Duration::from_secs(5), &mut waiter).await;
                if result.is_err() {
                    waiter.abort();
                    let _ = tokio::time::timeout(Duration::from_secs(1), waiter).await;
                }
                Some(result)
            }
            None => None,
        };
        let observers =
            tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
        // The server has bounded accept, header-read, gate and socket-write waits.
        // Join even when an assertion or the client timeout interrupted the journey.
        let server = tokio::task::spawn_blocking(move || server.join());
        let server = tokio::time::timeout(Duration::from_secs(16), server).await;
        let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(matches!(shutdown, Ok(Ok(()))), "task owner did not join");
            if let Some(result) = remaining {
                assert!(
                    matches!(result, Ok(Ok(_))),
                    "resolution waiter did not join"
                );
            }
            assert!(
                matches!(observers, Ok(Ok(()))),
                "queue observers did not join"
            );
            assert!(
                matches!(server, Ok(Ok(Ok(())))),
                "catalog server did not join successfully"
            );
            assert!(service.instances.tasks.status().is_idle());
            assert!(!service.installs.has_unsettled_effects());
            match journey {
                Ok(verify) => verify(),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }));
        if let Err(panic) = verification {
            eprintln!(
                "Retained catalog-await fixture (drift={drift}): {}",
                root.keep().display()
            );
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn dropped_resolution_caller_keeps_owned_fetch_until_shutdown() {
    use futures_util::FutureExt;
    use std::time::Duration;

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let (mut service, _) = open_fixture(root.path(), crate::library::LibraryId::new());
    let library = service.instances.directories().library().clone();
    let owner = service.instances.tasks.clone();
    let registry_revision = service.installs.snapshot().registry_revision;
    let (requested, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (catalog, server) = crate::catalog::tests::fixture_catalog(
        crate::catalog::tests::manifest(&[("1.21.11", "release")]),
        Some((requested, released)),
    );
    service.catalog = Arc::new(catalog);
    let service = Arc::new(service);
    let mut caller = None;
    let absent = |path: &std::path::Path| {
        std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    };
    let cache = root.path().join("cache/version_manifest_v2.json");
    let target = root.path().join("versions/1.21.11");
    let instances = root.path().join("instances");
    let canary = root.path().join("unrelated-user-file.txt");
    let journey = std::panic::AssertUnwindSafe(async {
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        operation.prepare_layout().unwrap();
        drop(operation);
        drop(pin);
        std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
        let initially_absent = absent(&cache) && absent(&target) && absent(&instances);
        let instances_before = service.instances.registry().list().unwrap();
        let pending_before = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let queue_before = service.installs.snapshot();
        caller = Some(tokio::spawn({
            let service = service.clone();
            async move { service.resolve("vanilla|1.21.11").await }
        }));
        tokio::time::timeout(Duration::from_secs(5), observed)
            .await
            .expect("catalog request was not observed")
            .expect("catalog request observer closed");
        let was_pending = !caller.as_ref().unwrap().is_finished();
        let accepted = owner.status();
        caller.as_ref().unwrap().abort();
        let caller_cancelled =
            tokio::time::timeout(Duration::from_secs(1), caller.as_mut().unwrap())
                .await
                .expect("disposable caller did not join")
                .is_err_and(|error| error.is_cancelled());
        drop(caller.take());
        let after_drop = owner.status();
        // The provider cannot deliver its body until cleanup sends release.
        let shutdown = owner.shutdown(Duration::from_secs(2)).await;
        let receipt = owner.shutdown_receipt();
        let receipted = receipt
            .as_ref()
            .is_some_and(|receipt| receipt.belongs_to(&owner));
        let held_through_shutdown = !server.is_finished();
        let absent_before_release = absent(&cache) && absent(&target) && absent(&instances);
        let instances_after = service.instances.registry().list().unwrap();
        let pending_after = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let queue_after = service.installs.snapshot();
        drop(receipt);
        move || {
            assert!(initially_absent && was_pending && caller_cancelled);
            assert_eq!(accepted.running.len(), 1);
            assert!(!accepted.closing && accepted.unsettled.is_empty());
            assert_eq!(
                after_drop, accepted,
                "dropping the caller must not cancel accepted work"
            );
            assert!(shutdown.is_ok() && receipted && held_through_shutdown);
            assert!(absent_before_release);
            assert!(instances_before.is_empty());
            assert_eq!(instances_after, instances_before);
            assert_eq!(pending_before, serde_json::json!([]));
            assert_eq!(pending_after, pending_before);
            assert_eq!(queue_after, queue_before);
        }
    })
    .catch_unwind()
    .await;
    let released = release.send(());
    service.installs.close_admission();
    let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(5)))
        .catch_unwind()
        .await;
    let remaining = match caller.take() {
        Some(mut caller) => {
            caller.abort();
            Some(
                tokio::time::timeout(Duration::from_secs(1), &mut caller)
                    .await
                    .is_ok_and(|result| result.is_err_and(|error| error.is_cancelled())),
            )
        }
        None => None,
    };
    let observers =
        tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
    let server = tokio::task::spawn_blocking(move || server.join());
    let server = tokio::time::timeout(Duration::from_secs(16), server).await;
    let final_state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (
            service.instances.registry().list().unwrap(),
            service.instances.pending().unwrap(),
            service.installs.snapshot(),
        )
    }));
    let receipt = owner.shutdown_receipt();
    let receipted = receipt
        .as_ref()
        .is_some_and(|receipt| receipt.belongs_to(&owner));
    let idle = owner.status().is_idle();
    let unsettled = service.installs.has_unsettled_effects();
    let runtime_settled = service.installs.runtime_cache().settle();
    drop(receipt);
    drop(service);
    drop(owner);
    let pins = library.wait_for_pins(Duration::from_secs(2)).await;
    let preserved = if matches!(&shutdown, Ok(Ok(())))
        && matches!(&observers, Ok(Ok(())))
        && matches!(&server, Ok(Ok(Ok(()))))
        && receipted
        && idle
        && !unsettled
        && runtime_settled.is_ok()
        && pins.is_ok()
    {
        Some(library.try_preserve())
    } else {
        None
    };
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            released.is_ok(),
            "server stopped holding the body before cleanup"
        );
        assert!(matches!(shutdown, Ok(Ok(()))), "task owner did not join");
        if let Some(result) = remaining {
            assert!(result, "disposable caller did not join");
        }
        assert!(
            matches!(observers, Ok(Ok(()))),
            "queue observers did not join"
        );
        assert!(
            matches!(server, Ok(Ok(Ok(())))),
            "catalog server did not join"
        );
        assert!(receipted && idle && !unsettled && runtime_settled.is_ok());
        assert!(
            pins.is_ok(),
            "profile capabilities remained after owned work joined"
        );
        assert!(
            matches!(preserved, Some(Ok(()))),
            "profile root did not preserve"
        );
        let (records, pending, queue) = final_state.unwrap();
        assert!(records.is_empty() && pending.is_empty());
        assert!(queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none());
        assert_eq!(queue.registry_revision, registry_revision);
        assert!(absent(&cache) && absent(&target) && absent(&instances));
        assert_eq!(
            std::fs::read(&canary).unwrap(),
            b"preserve unrelated user bytes\n"
        );
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained dropped-resolution fixture: {}",
            root.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn automatic_resolution_keeps_the_freshly_selected_provider_build() {
    use futures_util::FutureExt;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let (mut service, _) = open_fixture(root.path(), crate::library::LibraryId::new());
    let library = service.instances.directories().library().clone();
    let owner = service.instances.tasks.clone();
    let registry_revision = service.installs.snapshot().registry_revision;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    service.loader_build_url = Some(format!("{base}/v2/versions/loader/1.21.4").parse().unwrap());
    service.installs = Arc::new(
        (*service.installs)
            .clone()
            .with_loader_url(format!("{base}/unavailable-resolution").parse().unwrap()),
    );
    let (stop, stopped) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match stopped.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return requests,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < deadline,
                "resolution fixture was not stopped"
            );
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("resolution fixture accept failed: {error}"),
            };
            assert!(
                requests.len() < 4,
                "resolution fixture request bound exceeded"
            );
            stream.set_nonblocking(false).unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let mut length = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !request[..length]
                .windows(4)
                .any(|bytes| bytes == b"\r\n\r\n")
            {
                stream
                    .set_read_timeout(Some(
                        deadline.checked_duration_since(Instant::now()).unwrap(),
                    ))
                    .unwrap();
                assert!(
                    length < request.len(),
                    "resolution request headers exceed their bound"
                );
                let read = stream.read(&mut request[length..]).unwrap();
                assert!(read > 0, "resolution request ended before its headers");
                length += read;
            }
            if request[..length].starts_with(b"GET /v2/versions/loader/1.21.4 HTTP/1.1\r\n") {
                let body = br#"[{"loader":{"version":"0.16.14","stable":true,"maven":"net.fabricmc:fabric-loader:0.16.14"},"intermediary":{"version":"1.21.4","maven":"net.fabricmc:intermediary:1.21.4"},"launcherMeta":{"mainClass":{"client":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}}]"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
                requests.push("catalog");
            } else {
                assert!(request[..length].starts_with(b"GET /unavailable-resolution HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                requests.push("unavailable-resolution");
            }
        }
    });
    let service = Arc::new(service);
    let mut caller = None;
    let absent = |path: &std::path::Path| {
        std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    };
    let cache = root
        .path()
        .join("cache/loaders/catalog/component-fabric-builds-1.21.4.json");
    let canary = root.path().join("unrelated-user-file.txt");
    let version_id =
        axial_minecraft::installed_version_id_for(LoaderComponentId::Fabric, "1.21.4", "0.16.14")
            .unwrap();
    let journey = std::panic::AssertUnwindSafe(async {
        let pin = library.admit().unwrap();
        let original_generation = pin.generation();
        let original_library = pin.library_id();
        let operation = pin.managed_library().unwrap();
        operation.prepare_layout().unwrap();
        let initially_absent = absent(&cache)
            && absent(&root.path().join("versions").join(&version_id))
            && loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4")
                .unwrap()
                .is_none();
        let not_ready = matches!(
            service.installs.ready_version(&pin, &version_id).await,
            Err(crate::install::queue::InstallError::NotReady)
        );
        drop(operation);
        drop(pin);
        std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
        let instances_before = service.instances.registry().list().unwrap();
        let pending_before = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let queue_before = service.installs.snapshot();
        caller = Some(tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .resolve("loader_auto|net.fabricmc.fabric-loader|1.21.4")
                    .await
            }
        }));
        let joined = tokio::time::timeout(Duration::from_secs(5), caller.as_mut().unwrap())
            .await
            .expect("automatic resolution did not settle");
        drop(caller.take());
        let resolved = joined.unwrap().map(|(target, request, admission)| {
            let original = admission.generation().generation() == original_generation
                && admission.generation().library_id() == original_library;
            let valid = admission.validate().is_ok();
            (target, request, original, valid)
        });
        let cached_bytes = std::fs::read(&cache).unwrap();
        let explicit = tokio::time::timeout(
            Duration::from_secs(5),
            service.resolve(&format!(
                "loader_build|net.fabricmc.fabric-loader|{}",
                loaders::build_id_for(LoaderComponentId::Fabric, "1.21.4", "0.16.14")
            )),
        )
        .await
        .expect("explicit resolution did not settle")
        .map(|_| ());
        let created = if resolved.is_ok() {
            Some(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    service.create(
                        serde_json::from_value(serde_json::json!({
                            "name": "ProviderRefusal",
                            "selection_id": "loader_auto|net.fabricmc.fabric-loader|1.21.4"
                        }))
                        .unwrap(),
                    ),
                )
                .await
                .expect("creation did not settle")
                .map(|created| serde_json::to_value(created).unwrap()),
            )
        } else {
            None
        };
        let records_after = service.instances.registry().list().unwrap();
        let registered = records_after.first().is_some_and(|record| {
            service
                .instances
                .directories()
                .admit(&record.instance.id)
                .is_ok_and(|admitted| admitted.revalidate().is_ok())
        });
        let queue_after = service.installs.snapshot();
        move || {
            assert!(
                initially_absent && not_ready,
                "fixture must require a real provider build, not a Ready target"
            );
            assert!(instances_before.is_empty());
            assert_eq!(pending_before, serde_json::json!([]));
            assert_eq!(queue_after, queue_before);
            (
                resolved,
                explicit,
                created,
                records_after,
                registered,
                cached_bytes,
            )
        }
    })
    .catch_unwind()
    .await;
    let stopped = stop.send(());
    service.installs.close_admission();
    let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(15)))
        .catch_unwind()
        .await;
    let remaining = match caller.take() {
        Some(mut caller) => {
            caller.abort();
            Some(
                tokio::time::timeout(Duration::from_secs(1), &mut caller)
                    .await
                    .is_ok_and(|joined| match joined {
                        Ok(_) => true,
                        Err(error) => error.is_cancelled(),
                    }),
            )
        }
        None => None,
    };
    let observers =
        tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
    let server = tokio::task::spawn_blocking(move || server.join());
    let server = tokio::time::timeout(Duration::from_secs(18), server).await;
    let final_state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        (
            service.instances.registry().list().unwrap(),
            service.instances.pending().unwrap(),
            service.installs.snapshot(),
            loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4").unwrap(),
        )
    }));
    let receipt = owner.shutdown_receipt();
    let receipted = receipt
        .as_ref()
        .is_some_and(|receipt| receipt.belongs_to(&owner));
    let idle = owner.status().is_idle();
    let unsettled = service.installs.has_unsettled_effects();
    let runtime_settled = service.installs.runtime_cache().settle();
    drop(receipt);
    drop(service);
    drop(owner);
    let pins = library.wait_for_pins(Duration::from_secs(2)).await;
    let preserved = if matches!(&shutdown, Ok(Ok(())))
        && matches!(&observers, Ok(Ok(())))
        && matches!(&server, Ok(Ok(Ok(_))))
        && receipted
        && idle
        && !unsettled
        && runtime_settled.is_ok()
        && pins.is_ok()
    {
        Some(library.try_preserve())
    } else {
        None
    };
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(stopped.is_ok(), "resolution fixture stopped before cleanup");
        assert!(
            matches!(shutdown, Ok(Ok(()))),
            "resolution owner did not join"
        );
        assert!(
            remaining.is_none_or(|joined| joined),
            "disposable resolution caller did not join"
        );
        assert!(
            matches!(observers, Ok(Ok(()))),
            "queue observers did not join"
        );
        let requests = server
            .expect("HTTP server join timed out")
            .expect("HTTP server join task failed")
            .expect("HTTP server failed");
        assert!(receipted && idle && !unsettled && runtime_settled.is_ok());
        assert!(
            pins.is_ok(),
            "profile capabilities remained after resolution joined"
        );
        assert!(
            matches!(preserved, Some(Ok(()))),
            "resolution fixture root did not preserve"
        );
        let (records, pending, queue, cached) = final_state.unwrap();
        assert!(pending.is_empty());
        assert!(queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none());
        assert_eq!(queue.registry_revision, registry_revision);
        assert!(absent(&root.path().join("cache/version_manifest_v2.json")));
        assert!(
            std::fs::read_dir(root.path().join("versions"))
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(
            std::fs::read(&canary).unwrap(),
            b"preserve unrelated user bytes\n"
        );
        assert!(
            cache.is_file(),
            "the actual provider response was not persisted"
        );
        let (builds, state) = cached.expect("valid provider build was not cached");
        assert!(
            state.availability.fresh && state.availability.cache_hit && !state.availability.stale
        );
        assert_eq!(builds.len(), 1);
        let build = &builds[0];
        assert_eq!(
            loaders::parse_build_id(&build.build_id),
            Some((LoaderComponentId::Fabric, "1.21.4".into(), "0.16.14".into()))
        );
        assert_eq!(build.version_id, version_id);
        assert!(
            matches!(&build.install_source, loaders::LoaderInstallSource::ProfileJson { url } if url == "https://meta.fabricmc.net/v2/versions/loader/1.21.4/0.16.14/profile/json")
        );
        let (resolved, explicit, created, records_after, registered, cached_bytes) = match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        };
        let (target, request, original, valid) = resolved.unwrap_or_else(|error| {
            panic!("fresh automatic selection was lost when a later provider lookup was unavailable: {error:?}; served routes: {requests:?}")
        });
        assert_eq!(
            target.selection_id(),
            "loader_auto|net.fabricmc.fabric-loader|1.21.4"
        );
        assert_eq!(target.version_id(), version_id);
        assert_eq!(target.minecraft_version(), "1.21.4");
        assert_eq!(target.loader_key(), "fabric");
        assert_eq!(
            request,
            InstallQueueRequest::Loader {
                component_id: LoaderComponentId::Fabric,
                build_id: build.build_id.clone()
            }
        );
        assert!(
            original && valid,
            "resolution did not retain its valid original Admission"
        );
        assert_eq!(
            requests,
            [
                "catalog",
                "unavailable-resolution",
                "unavailable-resolution"
            ]
        );
        assert!(matches!(explicit, Err(InstanceError::VersionUnavailable)));
        let created = created.expect("creation control was not reached").unwrap();
        assert_eq!(
            created["view_model"],
            serde_json::json!({
                "state_id": "created_install_unavailable", "tone": "warn",
                "title": "Instance created",
                "summary": "Instance created. Installation could not be queued.",
                "detail": "Use Install on this instance to try again."
            })
        );
        assert!(created.get("install_queue").is_none());
        assert_eq!(records.len(), 1);
        assert_eq!(records, records_after);
        assert!(registered);
        assert_eq!(created["id"], records[0].instance.id.as_str());
        assert_eq!(records[0].instance.name, "ProviderRefusal");
        assert_eq!(records[0].instance.version_id, version_id);
        assert_eq!(std::fs::read(&cache).unwrap(), cached_bytes);
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained automatic resolution fixture: {}",
            root.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn shutdown_preserves_created_instance_while_install_provider_is_pending() {
    use futures_util::FutureExt;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let (mut service, _) = open_fixture(root.path(), crate::library::LibraryId::new());
    let library = service.instances.directories().library().clone();
    let owner = service.instances.tasks.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    service.loader_build_url = Some(format!("{base}/v2/versions/loader/1.21.4").parse().unwrap());
    service.installs = Arc::new(
        (*service.installs).clone().with_loader_url(
            format!("{base}/pending-install-resolution")
                .parse()
                .unwrap(),
        ),
    );
    let (requested, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut requested = Some(requested);
        let mut routes = Vec::new();
        for path in ["/v2/versions/loader/1.21.4", "/pending-install-resolution"] {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "creation provider request was not received"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("creation fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let mut length = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !request[..length]
                .windows(4)
                .any(|bytes| bytes == b"\r\n\r\n")
            {
                stream
                    .set_read_timeout(Some(
                        deadline.checked_duration_since(Instant::now()).unwrap(),
                    ))
                    .unwrap();
                assert!(
                    length < request.len(),
                    "creation request headers exceed their bound"
                );
                let read = stream.read(&mut request[length..]).unwrap();
                assert!(read > 0, "creation request ended before its headers");
                length += read;
            }
            assert!(request[..length].starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()));
            let body = br#"[{"loader":{"version":"0.16.14","stable":true,"maven":"net.fabricmc:fabric-loader:0.16.14"},"intermediary":{"version":"1.21.4","maven":"net.fabricmc:intermediary:1.21.4"},"launcherMeta":{"mainClass":{"client":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}}]"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            routes.push(path);
            let held = path == "/pending-install-resolution";
            if held {
                requested
                    .take()
                    .unwrap()
                    .send(())
                    .expect("creation request observer dropped");
                released
                    .recv_timeout(Duration::from_secs(8))
                    .expect("creation provider body was not released");
            }
            match stream.write_all(body) {
                Ok(()) => {}
                Err(error)
                    if held
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ) => {}
                Err(error) => panic!("creation provider body write failed: {error}"),
            }
        }
        routes
    });
    let service = Arc::new(service);
    let mut caller = None;
    let cache = root
        .path()
        .join("cache/loaders/catalog/component-fabric-builds-1.21.4.json");
    let canary = root.path().join("unrelated-user-file.txt");
    let version_id =
        axial_minecraft::installed_version_id_for(LoaderComponentId::Fabric, "1.21.4", "0.16.14")
            .unwrap();
    let queue_before = service.installs.snapshot();
    let journey = std::panic::AssertUnwindSafe(async {
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        operation.prepare_layout().unwrap();
        let cache_absent = std::fs::symlink_metadata(&cache)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            && loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4")
                .unwrap()
                .is_none();
        let not_ready = matches!(
            service.installs.ready_version(&pin, &version_id).await,
            Err(crate::install::queue::InstallError::NotReady)
        );
        drop(operation);
        drop(pin);
        std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
        let initially_empty = service.instances.registry().list().unwrap().is_empty()
            && service.instances.pending().unwrap().is_empty();
        caller = Some(tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .create(
                        serde_json::from_value(serde_json::json!({
                            "name": "PendingInstallProvider",
                            "selection_id": "loader_auto|net.fabricmc.fabric-loader|1.21.4"
                        }))
                        .unwrap(),
                    )
                    .await
                    .map(|created| serde_json::to_value(created).unwrap())
            }
        }));
        tokio::time::timeout(Duration::from_secs(5), observed)
            .await
            .expect("postcommit provider request was not observed")
            .expect("provider observer closed");
        let records = service.instances.registry().list().unwrap();
        let record = records
            .first()
            .expect("provider request preceded registry commit");
        let live = service
            .instances
            .registry()
            .get_live(&record.instance.id)
            .unwrap();
        let registered = service
            .instances
            .directories()
            .admit(&record.instance.id)
            .is_ok_and(|admitted| admitted.revalidate().is_ok());
        let pending_empty = service.instances.pending().unwrap().is_empty();
        let cache_at_commit = std::fs::read(&cache).unwrap();
        let queue_at_commit = service.installs.snapshot();
        let held = !server.is_finished() && !caller.as_ref().unwrap().is_finished();
        let accepted = owner.status();
        let held_shutdown = owner.shutdown(Duration::from_secs(2)).await;
        let receipt = owner.shutdown_receipt();
        let held_receipted = receipt
            .as_ref()
            .is_some_and(|receipt| receipt.belongs_to(&owner));
        drop(receipt);
        let response = if held_shutdown.is_ok() {
            let joined =
                tokio::time::timeout(Duration::from_secs(1), caller.as_mut().unwrap()).await;
            match joined {
                Ok(joined) => {
                    drop(caller.take());
                    Some(joined.unwrap())
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let body_still_held = !server.is_finished();
        let records_after_shutdown = service.instances.registry().list().unwrap();
        let queue_after_shutdown = service.installs.snapshot();
        let cache_after_shutdown = std::fs::read(&cache).unwrap();
        move || {
            assert!(cache_absent && not_ready && initially_empty);
            assert!(
                held && body_still_held,
                "postcommit provider body escaped its gate"
            );
            assert!(
                !accepted.closing && accepted.unsettled.is_empty() && !accepted.running.is_empty()
            );
            assert_eq!(records.len(), 1);
            assert_eq!(records[0], live);
            assert!(
                registered && pending_empty,
                "held provider must follow complete registry and namespace publication"
            );
            assert_eq!(records_after_shutdown, records);
            assert_eq!(queue_after_shutdown, queue_at_commit);
            assert_eq!(cache_after_shutdown, cache_at_commit);
            (
                held_shutdown,
                held_receipted,
                response,
                records,
                queue_at_commit,
                cache_at_commit,
            )
        }
    })
    .catch_unwind()
    .await;
    service.installs.close_admission();
    let released = release.send(());
    let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(15)))
        .catch_unwind()
        .await;
    let remaining = match caller.take() {
        Some(mut caller) => match tokio::time::timeout(Duration::from_secs(5), &mut caller).await {
            Ok(Ok(_)) => true,
            Ok(Err(_)) => false,
            Err(_) => {
                caller.abort();
                let _ = tokio::time::timeout(Duration::from_secs(1), caller).await;
                false
            }
        },
        None => true,
    };
    let observers =
        tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
    let server = tokio::task::spawn_blocking(move || server.join());
    let server = tokio::time::timeout(Duration::from_secs(18), server).await;
    let final_state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let records = service.instances.registry().list().unwrap();
        let registered = records.first().is_some_and(|record| {
            service
                .instances
                .directories()
                .admit(&record.instance.id)
                .is_ok_and(|admitted| admitted.revalidate().is_ok())
        });
        (
            records,
            registered,
            service.instances.pending().unwrap(),
            service.installs.snapshot(),
            loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4").unwrap(),
        )
    }));
    let receipt = owner.shutdown_receipt();
    let receipted = receipt
        .as_ref()
        .is_some_and(|receipt| receipt.belongs_to(&owner));
    let idle = owner.status().is_idle();
    let unsettled = service.installs.has_unsettled_effects()
        || service.instances.has_unsettled_effects()
        || service.instances.has_pending_intents();
    let runtime_settled = service.installs.runtime_cache().settle();
    drop(receipt);
    drop(service);
    drop(owner);
    let pins = library.wait_for_pins(Duration::from_secs(2)).await;
    let preserved = if matches!(&shutdown, Ok(Ok(())))
        && matches!(&observers, Ok(Ok(())))
        && matches!(&server, Ok(Ok(Ok(_))))
        && receipted
        && idle
        && !unsettled
        && runtime_settled.is_ok()
        && pins.is_ok()
    {
        Some(library.try_preserve())
    } else {
        None
    };
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(released.is_ok(), "provider body gate ended before cleanup");
        assert!(
            matches!(shutdown, Ok(Ok(()))),
            "creation owner did not join after body release"
        );
        assert!(remaining, "live creation caller did not join");
        assert!(
            matches!(observers, Ok(Ok(()))),
            "queue observers did not join"
        );
        let routes = server
            .expect("provider server join timed out")
            .expect("provider join task failed")
            .expect("provider server failed");
        assert_eq!(
            routes,
            ["/v2/versions/loader/1.21.4", "/pending-install-resolution"]
        );
        assert!(receipted && idle && !unsettled && runtime_settled.is_ok());
        assert!(
            pins.is_ok(),
            "profile capabilities remained after creation joined"
        );
        assert!(
            matches!(preserved, Some(Ok(()))),
            "creation fixture root did not preserve"
        );
        let (records, registered, pending, queue, cached) = final_state.unwrap();
        let (held_shutdown, held_receipted, response, committed, queue_at_commit, cache_at_commit) =
            match journey {
                Ok(verify) => verify(),
                Err(panic) => std::panic::resume_unwind(panic),
            };
        assert_eq!(records, committed);
        assert!(registered && pending.is_empty());
        assert_eq!(records[0].instance.name, "PendingInstallProvider");
        assert_eq!(records[0].instance.version_id, version_id);
        assert_eq!(queue_at_commit, queue_before);
        assert!(queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none());
        assert_eq!(queue.registry_revision, queue_before.registry_revision);
        assert_eq!(std::fs::read(&cache).unwrap(), cache_at_commit);
        let (builds, state) = cached.expect("initial real Fabric response was not cached");
        assert!(
            state.availability.fresh && state.availability.cache_hit && !state.availability.stale
        );
        assert_eq!(builds.len(), 1);
        assert_eq!(
            loaders::parse_build_id(&builds[0].build_id),
            Some((LoaderComponentId::Fabric, "1.21.4".into(), "0.16.14".into()))
        );
        assert_eq!(builds[0].version_id, version_id);
        assert_eq!(
            std::fs::read(&canary).unwrap(),
            b"preserve unrelated user bytes\n"
        );
        for name in ["versions", "libraries", "assets"] {
            assert!(
                std::fs::read_dir(root.path().join(name))
                    .unwrap()
                    .next()
                    .is_none()
            );
        }
        let instance_root = root
            .path()
            .join("instances")
            .join(&records[0].directory_name);
        let mut names = std::fs::read_dir(&instance_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "config",
                "logs",
                "mods",
                "resourcepacks",
                "saves",
                "screenshots",
                "shaderpacks"
            ]
            .map(std::ffi::OsString::from)
        );
        for name in names {
            assert!(
                std::fs::read_dir(instance_root.join(name))
                    .unwrap()
                    .next()
                    .is_none()
            );
        }
        assert!(
            held_shutdown.is_ok() && held_receipted,
            "postcommit create did not settle on owner shutdown while the provider body remained withheld: {held_shutdown:?}"
        );
        let created = response
            .expect("live create caller did not return before body release")
            .unwrap();
        assert_eq!(created["id"], records[0].instance.id.as_str());
        assert_eq!(
            created["view_model"],
            serde_json::json!({
                "state_id": "created_install_unavailable", "tone": "warn", "title": "Instance created",
                "summary": "Instance created. Installation could not be queued.",
                "detail": "Use Install on this instance to try again."
            })
        );
        assert!(created.get("install_queue").is_none());
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained postcommit creation shutdown fixture: {}",
            root.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn explicit_build_resolution_cancels_without_publishing() {
    use futures_util::FutureExt;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    for cancel_fetch in [false, true] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (mut service, _) = open_fixture(root.path(), crate::library::LibraryId::new());
        let library = service.instances.directories().library().clone();
        let owner = service.instances.tasks.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        service.installs = Arc::new(
            (*service.installs).clone().with_loader_url(
                format!(
                    "http://{}/v2/versions/loader/1.21.4",
                    listener.local_addr().unwrap()
                )
                .parse()
                .unwrap(),
            ),
        );
        let (requested, observed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "explicit build request was not received"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("explicit build fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let mut length = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !request[..length]
                .windows(4)
                .any(|bytes| bytes == b"\r\n\r\n")
            {
                stream
                    .set_read_timeout(Some(
                        deadline.checked_duration_since(Instant::now()).unwrap(),
                    ))
                    .unwrap();
                assert!(
                    length < request.len(),
                    "explicit build headers exceed their bound"
                );
                let read = stream.read(&mut request[length..]).unwrap();
                assert!(read > 0, "explicit build request ended before its headers");
                length += read;
            }
            assert!(request[..length].starts_with(b"GET /v2/versions/loader/1.21.4 HTTP/1.1\r\n"));
            let body = br#"[{"loader":{"version":"0.16.14","stable":true,"maven":"net.fabricmc:fabric-loader:0.16.14"},"intermediary":{"version":"1.21.4","maven":"net.fabricmc:intermediary:1.21.4"},"launcherMeta":{"mainClass":{"client":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}}]"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            requested.send(()).expect("explicit build observer dropped");
            released
                .recv_timeout(Duration::from_secs(8))
                .expect("explicit build body was not released");
            match stream.write_all(body) {
                Ok(()) => true,
                Err(error)
                    if cancel_fetch
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ) =>
                {
                    false
                }
                Err(error) => panic!("explicit build body write failed: {error}"),
            }
        });
        let service = Arc::new(service);
        let mut release = Some(release);
        let mut caller = None;
        let absent = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        };
        let cache = root
            .path()
            .join("cache/loaders/catalog/component-fabric-builds-1.21.4.json");
        let canary = root.path().join("unrelated-user-file.txt");
        let build_id = loaders::build_id_for(LoaderComponentId::Fabric, "1.21.4", "0.16.14");
        let selection_id = format!("loader_build|net.fabricmc.fabric-loader|{build_id}");
        let version_id = axial_minecraft::installed_version_id_for(
            LoaderComponentId::Fabric,
            "1.21.4",
            "0.16.14",
        )
        .unwrap();
        let queue_before = service.installs.snapshot();
        let journey = std::panic::AssertUnwindSafe(async {
            let pin = library.admit().unwrap();
            let original_generation = pin.generation();
            let original_library = pin.library_id();
            let operation = pin.managed_library().unwrap();
            operation.prepare_layout().unwrap();
            let initially_absent = absent(&cache)
                && absent(&root.path().join("instances"))
                && absent(&root.path().join("cache/version_manifest_v2.json"))
                && loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4")
                    .unwrap()
                    .is_none();
            let not_ready = matches!(
                service.installs.ready_version(&pin, &version_id).await,
                Err(crate::install::queue::InstallError::NotReady)
            );
            drop(operation);
            drop(pin);
            std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
            caller = Some(tokio::spawn({
                let service = service.clone();
                let selection_id = selection_id.clone();
                async move { service.resolve(&selection_id).await }
            }));
            tokio::time::timeout(Duration::from_secs(5), observed)
                .await
                .expect("explicit build request was not observed")
                .expect("explicit build observer closed");
            let held = !server.is_finished() && !caller.as_ref().unwrap().is_finished();
            let records_held = service.instances.registry().list().unwrap();
            let pending_held = service.instances.pending().unwrap();
            let queue_held = service.installs.snapshot();
            let accepted = owner.status();
            let held_shutdown = if cancel_fetch {
                Some(owner.shutdown(Duration::from_secs(2)).await)
            } else {
                release.take().unwrap().send(()).unwrap();
                None
            };
            let receipt = owner.shutdown_receipt();
            let held_receipted = receipt
                .as_ref()
                .is_some_and(|receipt| receipt.belongs_to(&owner));
            drop(receipt);
            let result = if held_shutdown.as_ref().is_none_or(|result| result.is_ok()) {
                match tokio::time::timeout(Duration::from_secs(2), caller.as_mut().unwrap()).await {
                    Ok(joined) => {
                        drop(caller.take());
                        Some(joined.unwrap().map(|(target, request, admission)| {
                            let original = admission.generation().generation()
                                == original_generation
                                && admission.generation().library_id() == original_library;
                            let valid = admission.validate().is_ok();
                            (target, request, original, valid)
                        }))
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            let body_still_held = !server.is_finished();
            let final_pin = library.admit().unwrap();
            let same_generation = final_pin.generation() == original_generation
                && final_pin.library_id() == original_library
                && final_pin.revalidate().is_ok();
            drop(final_pin);
            move || {
                assert!(initially_absent && not_ready && held && same_generation);
                assert!(
                    !accepted.closing
                        && accepted.unsettled.is_empty()
                        && !accepted.running.is_empty()
                );
                assert!(records_held.is_empty() && pending_held.is_empty());
                (
                    held_shutdown,
                    held_receipted,
                    body_still_held,
                    result,
                    queue_held,
                )
            }
        })
        .catch_unwind()
        .await;
        service.installs.close_admission();
        let released = release.take().map(|release| release.send(()));
        let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(15)))
            .catch_unwind()
            .await;
        let remaining = match caller.take() {
            Some(mut caller) => {
                match tokio::time::timeout(Duration::from_secs(5), &mut caller).await {
                    Ok(Ok(_)) => true,
                    Ok(Err(_)) => false,
                    Err(_) => {
                        caller.abort();
                        let _ = tokio::time::timeout(Duration::from_secs(1), caller).await;
                        false
                    }
                }
            }
            None => true,
        };
        let observers =
            tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
        let server = tokio::task::spawn_blocking(move || server.join());
        let server = tokio::time::timeout(Duration::from_secs(18), server).await;
        let final_state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let pin = library.admit().unwrap();
            let operation = pin.managed_library().unwrap();
            (
                service.instances.registry().list().unwrap(),
                service.instances.pending().unwrap(),
                service.installs.snapshot(),
                loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4")
                    .unwrap()
                    .is_none(),
            )
        }));
        let receipt = owner.shutdown_receipt();
        let receipted = receipt
            .as_ref()
            .is_some_and(|receipt| receipt.belongs_to(&owner));
        let idle = owner.status().is_idle();
        let unsettled = service.installs.has_unsettled_effects()
            || service.instances.has_unsettled_effects()
            || service.instances.has_pending_intents();
        let runtime_settled = service.installs.runtime_cache().settle();
        drop(receipt);
        drop(service);
        drop(owner);
        let pins = library.wait_for_pins(Duration::from_secs(2)).await;
        let preserved = if matches!(&shutdown, Ok(Ok(())))
            && matches!(&observers, Ok(Ok(())))
            && matches!(&server, Ok(Ok(Ok(_))))
            && receipted
            && idle
            && !unsettled
            && runtime_settled.is_ok()
            && pins.is_ok()
        {
            Some(library.try_preserve())
        } else {
            None
        };
        let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                released.is_none_or(|released| released.is_ok()),
                "explicit build gate ended before cleanup"
            );
            assert!(
                matches!(shutdown, Ok(Ok(()))),
                "explicit resolution owner did not join after release"
            );
            assert!(remaining, "live explicit resolution caller did not join");
            assert!(
                matches!(observers, Ok(Ok(()))),
                "queue observers did not join"
            );
            let delivered = server
                .expect("provider server join timed out")
                .expect("provider join task failed")
                .expect("provider server failed");
            assert!(
                cancel_fetch || delivered,
                "positive provider body was not delivered"
            );
            assert!(receipted && idle && !unsettled && runtime_settled.is_ok());
            assert!(
                pins.is_ok(),
                "profile capabilities remained after explicit resolution joined"
            );
            assert!(
                matches!(preserved, Some(Ok(()))),
                "explicit resolution fixture root did not preserve"
            );
            let (records, pending, queue, cache_absent) = final_state.unwrap();
            assert!(records.is_empty() && pending.is_empty() && cache_absent);
            assert!(
                queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none()
            );
            assert_eq!(queue.registry_revision, queue_before.registry_revision);
            assert!(
                absent(&cache)
                    && absent(&root.path().join("instances"))
                    && absent(&root.path().join("cache/version_manifest_v2.json"))
            );
            for name in ["versions", "libraries", "assets", "cache/loaders/catalog"] {
                assert!(
                    std::fs::read_dir(root.path().join(name))
                        .unwrap()
                        .next()
                        .is_none()
                );
            }
            assert_eq!(
                std::fs::read(&canary).unwrap(),
                b"preserve unrelated user bytes\n"
            );
            let (held_shutdown, held_receipted, body_still_held, result, queue_held) = match journey
            {
                Ok(verify) => verify(),
                Err(panic) => std::panic::resume_unwind(panic),
            };
            assert_eq!(queue_held, queue_before);
            if cancel_fetch {
                assert!(
                    body_still_held,
                    "explicit provider body escaped the cancellation gate"
                );
                let held_shutdown = held_shutdown.unwrap();
                assert!(
                    held_shutdown.is_ok() && held_receipted,
                    "explicit build resolution did not settle while its provider body remained withheld: {held_shutdown:?}"
                );
                assert!(
                    matches!(result, Some(Err(InstanceError::Cancelled))),
                    "live explicit resolution caller did not return typed cancellation before release: {result:?}"
                );
            } else {
                let (target, request, original, valid) = result
                    .expect("positive explicit resolution did not return")
                    .unwrap();
                assert_eq!(target.selection_id(), selection_id);
                assert_eq!(target.version_id(), version_id);
                assert_eq!(target.minecraft_version(), "1.21.4");
                assert_eq!(target.loader_key(), "fabric");
                assert_eq!(
                    request,
                    InstallQueueRequest::Loader {
                        component_id: LoaderComponentId::Fabric,
                        build_id
                    }
                );
                assert!(
                    original && valid,
                    "explicit resolution did not retain its valid original Admission"
                );
            }
        }));
        if let Err(panic) = verification {
            eprintln!(
                "Retained explicit resolution fixture (cancel_fetch={cancel_fetch}): {}",
                root.keep().display()
            );
            std::panic::resume_unwind(panic);
        }
    }
}

#[test]
fn loader_picker_retains_owned_fetches_and_rejects_library_drift() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let retain_runtime = std::cell::Cell::new(false);
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(loader_picker_fetches(&retain_runtime))
    }));
    if retain_runtime.get() {
        std::mem::forget(runtime);
    }
    if let Err(panic) = checked {
        std::panic::resume_unwind(panic);
    }
}

async fn loader_picker_fetches(retain_runtime: &std::cell::Cell<bool>) {
    use futures_util::FutureExt;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    for (cancel_fetch, drift) in [(false, false), (true, false), (false, true)] {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (mut service, accounts) = open_fixture(root.path(), crate::library::LibraryId::new());
        let library = service.instances.directories().library().clone();
        let owner = service.instances.tasks.clone();
        let registry_revision = service.installs.snapshot().registry_revision;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        service.loader_build_url = Some(
            format!(
                "http://{}/v2/versions/loader/1.21.4",
                listener.local_addr().unwrap()
            )
            .parse()
            .unwrap(),
        );
        let (requested, observed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "loader request was not received");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("loader fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let mut length = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !request[..length]
                .windows(4)
                .any(|bytes| bytes == b"\r\n\r\n")
            {
                stream
                    .set_read_timeout(Some(
                        deadline.checked_duration_since(Instant::now()).unwrap(),
                    ))
                    .unwrap();
                assert!(
                    length < request.len(),
                    "loader request headers exceed their bound"
                );
                let read = stream.read(&mut request[length..]).unwrap();
                assert!(read > 0, "loader request ended before its headers");
                length += read;
            }
            assert!(request[..length].starts_with(b"GET /v2/versions/loader/1.21.4 HTTP/1.1\r\n"));
            let body = br#"[{"loader":{"version":"0.16.14","stable":true,"maven":"net.fabricmc:fabric-loader:0.16.14"},"intermediary":{"version":"1.21.4","maven":"net.fabricmc:intermediary:1.21.4"},"launcherMeta":{"mainClass":{"client":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}}]"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            requested.send(()).expect("loader request observer dropped");
            released
                .recv_timeout(Duration::from_secs(8))
                .expect("loader body was not released");
            match stream.write_all(body) {
                Ok(()) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    false
                }
                Err(error) => panic!("loader body write failed: {error}"),
            }
        });
        let service = Arc::new(service);
        let mut release = Some(release);
        let mut caller = None;
        let absent = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        };
        let cache = root
            .path()
            .join("cache/loaders/catalog/component-fabric-builds-1.21.4.json");
        let canary = root.path().join("unrelated-user-file.txt");
        let journey = std::panic::AssertUnwindSafe(async {
            let pin = library.admit().unwrap();
            let original_generation = pin.generation();
            let operation = pin.managed_library().unwrap();
            operation.prepare_layout().unwrap();
            let versions = pin.directory().unwrap()
                .open_directory(&axial_fs::LeafName::new("versions").unwrap()).unwrap();
            let revision_before = versions.revision().unwrap();
            let cache_before = loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4").unwrap();
            drop(operation);
            drop(pin);
            std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
            let initially_absent = absent(&cache) && absent(&root.path().join("instances"));
            let instances_before = service.instances.registry().list().unwrap();
            let pending_before = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
            let queue_before = service.installs.snapshot();
            caller = Some(tokio::spawn({
                let service = service.clone();
                async move {
                    service
                        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
                        .await
                }
            }));
            let observed = tokio::time::timeout(Duration::from_secs(5), observed).await;
            if !matches!(observed, Ok(Ok(()))) && caller.as_ref().unwrap().is_finished() {
                let result = caller.take().unwrap().await.map(|result| result.map(|_| ()));
                panic!("loader picker finished before provider request: {result:?}");
            }
            observed
                .expect("loader HTTP request was not observed")
                .expect("loader request observer closed");
            let held = !caller.as_ref().unwrap().is_finished() && !server.is_finished();
            let accepted = owner.status();
            let external = root.path().join("versions/external-degraded-entry");
            let metadata = external.join("external-degraded-entry.json");
            let malformed = b"{not valid external version metadata\n";
            assert!(absent(&external));
            let changed_revision = if drift {
                std::fs::create_dir(&external).unwrap();
                std::fs::write(&metadata, malformed).unwrap();
                Some(tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let revision = versions.revision().unwrap();
                        if revision != revision_before { break revision; }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await)
            } else { None };
            drop(versions);
            let mut view = None;
            let mut refused = None;
            let mut held_shutdown = None;
            if cancel_fetch {
                caller.as_ref().unwrap().abort();
                let joined = tokio::time::timeout(Duration::from_secs(1), caller.as_mut().unwrap())
                    .await
                    .expect("disposable loader caller did not join");
                drop(caller.take());
                let after_drop = owner.status();
                let shutdown = owner.shutdown(Duration::from_secs(2)).await;
                let receipt = owner.shutdown_receipt();
                let receipted = receipt.as_ref().is_some_and(|receipt| receipt.belongs_to(&owner));
                held_shutdown = Some((joined.is_err_and(|error| error.is_cancelled()), after_drop, shutdown, receipted, !server.is_finished()));
            } else {
                release.take().unwrap().send(()).unwrap();
                let joined = tokio::time::timeout(Duration::from_secs(5), caller.as_mut().unwrap())
                    .await
                    .expect("loader picker did not settle");
                drop(caller.take());
                let result = joined.unwrap();
                if drift {
                    refused = result.err();
                } else {
                    view = Some(result.unwrap());
                }
            }
            let pin = library.admit().unwrap();
            let final_generation = pin.generation();
            let operation = pin.managed_library().unwrap();
            let cache_after = loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4").unwrap();
            drop(operation);
            drop(pin);
            let preserved_external = if drift {
                let bytes = std::fs::read(&metadata).unwrap();
                std::fs::remove_file(&metadata).unwrap();
                std::fs::remove_dir(&external).unwrap();
                view = Some(tokio::time::timeout(Duration::from_secs(5), service.loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4"))
                    .await.expect("restored loader picker did not settle").unwrap());
                Some(bytes)
            } else { None };
            let instances_after = service.instances.registry().list().unwrap();
            let pending_after = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
            let queue_after = service.installs.snapshot();
            move || {
                assert!(initially_absent && cache_before.is_none() && held);
                assert_eq!(accepted.running.len(), 1);
                assert!(!accepted.closing && accepted.unsettled.is_empty());
                assert_eq!(final_generation, original_generation);
                if drift {
                    assert_ne!(changed_revision.unwrap().unwrap(), revision_before);
                    assert!(matches!(refused, Some(InstanceError::VersionUnavailable)), "{refused:?}");
                    assert_eq!(preserved_external.as_deref(), Some(malformed.as_slice()));
                } else {
                    assert!(changed_revision.is_none() && preserved_external.is_none());
                }
                assert!(absent(&external));
                if let Some((caller_cancelled, after_drop, shutdown, receipted, body_held)) = held_shutdown {
                    assert!(caller_cancelled);
                    assert_eq!(after_drop, accepted, "dropping the caller must not cancel accepted work");
                    assert!(body_held, "provider body escaped the cancellation gate");
                    assert!(shutdown.is_ok() && receipted, "loader fetch did not settle on owner cancellation while its body remained withheld");
                    assert!(cache_after.is_none(), "cancelled provider fetch published a cache");
                } else {
                    let view = view.unwrap();
                    assert_eq!(view.source_id, "net.fabricmc.fabric-loader");
                    assert_eq!(view.minecraft_version_id, "1.21.4");
                    assert_eq!(
                        view.auto.selection_id,
                        "loader_auto|net.fabricmc.fabric-loader|1.21.4"
                    );
                    assert!(view.auto.enabled);
                    assert_eq!(view.builds.len(), 1);
                    let build = &view.builds[0];
                    assert_eq!(loaders::parse_build_id(&build.build_id), Some((LoaderComponentId::Fabric, "1.21.4".into(), "0.16.14".into())));
                    assert_eq!(build.label, "0.16.14");
                    assert!(build.enabled && !build.installed && build.recommended);
                    let (cached, state) = cache_after.expect("valid provider body was not cached");
                    assert!(state.availability.fresh && state.availability.cache_hit && !state.availability.stale);
                    assert_eq!(cached.len(), 1);
                    assert_eq!(cached[0].build_id, build.build_id);
                    assert!(matches!(&cached[0].install_source, loaders::LoaderInstallSource::ProfileJson { url } if url == "https://meta.fabricmc.net/v2/versions/loader/1.21.4/0.16.14/profile/json"));
                }
                assert!(instances_before.is_empty());
                assert_eq!(instances_after, instances_before);
                assert_eq!(pending_before, serde_json::json!([]));
                assert_eq!(pending_after, pending_before);
                assert_eq!(queue_after, queue_before);
            }
        })
        .catch_unwind()
        .await;
        let released = release.take().map(|release| release.send(()));
        service.installs.close_admission();
        let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(15)))
            .catch_unwind()
            .await;
        let remaining = match caller.take() {
            Some(mut caller) => {
                caller.abort();
                let joined = tokio::time::timeout(Duration::from_secs(1), &mut caller)
                    .await
                    .is_ok_and(|joined| match joined {
                        Ok(_) => true,
                        Err(error) => error.is_cancelled(),
                    });
                if !joined {
                    std::mem::forget(caller);
                }
                Some(joined)
            }
            None => None,
        };
        let observers =
            tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
        let server = tokio::task::spawn_blocking(move || server.join());
        let server = tokio::time::timeout(Duration::from_secs(18), server).await;
        let final_state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let pin = library.admit().unwrap();
            let operation = pin.managed_library().unwrap();
            (
                service.instances.registry().list().unwrap(),
                service.instances.pending().unwrap(),
                service.installs.snapshot(),
                loaders::fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.4")
                    .unwrap()
                    .is_none(),
            )
        }));
        let receipt = owner.shutdown_receipt();
        let receipted = receipt
            .as_ref()
            .is_some_and(|receipt| receipt.belongs_to(&owner));
        let idle = owner.status().is_idle();
        let unsettled = service.installs.has_unsettled_effects();
        let runtime_settled = service.installs.runtime_cache().settle();
        drop(receipt);
        let settled = matches!(&shutdown, Ok(Ok(())))
            && matches!(&observers, Ok(Ok(())))
            && matches!(&server, Ok(Ok(Ok(_))))
            && remaining.is_none_or(|joined| joined)
            && receipted
            && idle
            && !unsettled
            && runtime_settled.is_ok();
        if settled {
            drop(service);
            drop(owner);
            drop(accounts);
        } else {
            retain_runtime.set(true);
            std::mem::forget(service);
            std::mem::forget(owner);
            std::mem::forget(accounts);
        }
        let pins = if settled {
            Some(library.wait_for_pins(Duration::from_secs(2)).await)
        } else {
            None
        };
        let preserved = matches!(&pins, Some(Ok(()))).then(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| library.try_preserve()))
        });
        if !matches!(&preserved, Some(Ok(Ok(())))) {
            retain_runtime.set(true);
            std::mem::forget(library.clone());
        }
        let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let verify = match journey {
                Ok(verify) => verify,
                Err(panic) => std::panic::resume_unwind(panic),
            };
            if let Some(released) = released {
                assert!(released.is_ok(), "loader body gate ended before cleanup");
            }
            assert!(
                matches!(shutdown, Ok(Ok(()))),
                "loader owner did not join after release"
            );
            assert!(
                remaining.is_none_or(|joined| joined),
                "disposable loader caller did not join"
            );
            assert!(
                matches!(observers, Ok(Ok(()))),
                "queue observers did not join"
            );
            assert!(
                matches!(server, Ok(Ok(Ok(_)))),
                "loader HTTP server did not join"
            );
            if !cancel_fetch {
                assert!(
                    matches!(server, Ok(Ok(Ok(true)))),
                    "positive provider body was not delivered"
                );
            }
            assert!(receipted && idle && !unsettled && runtime_settled.is_ok());
            assert!(
                matches!(pins, Some(Ok(()))),
                "profile capabilities remained after loader work joined"
            );
            match preserved {
                Some(Ok(Ok(()))) => {}
                Some(Err(panic)) => std::panic::resume_unwind(panic),
                _ => panic!("loader fixture root did not preserve"),
            }
            let (records, pending, queue, cache_absent) = final_state.unwrap();
            assert!(records.is_empty() && pending.is_empty());
            assert!(
                queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none()
            );
            assert_eq!(queue.registry_revision, registry_revision);
            assert!(absent(&root.path().join("instances")));
            assert!(absent(&root.path().join("cache/version_manifest_v2.json")));
            assert!(
                std::fs::read_dir(root.path().join("versions"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert_eq!(
                std::fs::read(&canary).unwrap(),
                b"preserve unrelated user bytes\n"
            );
            verify();
            assert_eq!(cache_absent, cancel_fetch);
            assert_eq!(absent(&cache), cancel_fetch);
        }));
        if let Err(panic) = verification {
            let _ = writeln!(
                std::io::stderr(),
                "Retained loader picker fixture (cancel_fetch={cancel_fetch}, drift={drift}): {}",
                root.keep().display()
            );
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn dropped_loader_versions_caller_keeps_owned_fetch_until_shutdown() {
    use axial_minecraft::loaders::types::CachedCatalog;
    use futures_util::FutureExt;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    for (hold_manifest, cancel_fetch) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (mut service, _) = open_fixture(root.path(), crate::library::LibraryId::new());
        let library = service.instances.directories().library().clone();
        let owner = service.instances.tasks.clone();
        let manifest =
            crate::catalog::tests::manifest(&[("25w01a", "snapshot"), ("1.21.4", "release")]);
        let cache = root
            .path()
            .join("cache/loaders/catalog/component-fabric-supported-versions.json");
        let manifest_cache = root.path().join("cache/version_manifest_v2.json");
        let missing_cache = if hold_manifest {
            &manifest_cache
        } else {
            &cache
        };
        let raw: Vec<axial_minecraft::LoaderGameVersion> =
            serde_json::from_value(serde_json::json!([
                {"id":"1.21.4","stable_hint":true}, {"id":"25w01a","stable_hint":false}
            ]))
            .unwrap();
        let canary = root.path().join("unrelated-user-file.txt");
        let absent = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        };
        let queue_before = service.installs.snapshot();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        service.loader_game_url = Some(
            format!("http://{}/v2/versions/game", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let (request_line, body): (&[u8], Vec<u8>) = if hold_manifest {
            service.loader_manifest_url = Some(
                format!("http://{}/manifest.json", listener.local_addr().unwrap())
                    .parse()
                    .unwrap(),
            );
            (b"GET /manifest.json HTTP/1.1\r\n", manifest.clone())
        } else {
            (
                b"GET /v2/versions/game HTTP/1.1\r\n",
                br#"[{"version":"1.21.4","stable":true},{"version":"25w01a","stable":false}]"#
                    .to_vec(),
            )
        };
        let (requested, observed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "supported-version request was not received"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("supported-version accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            let mut length = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !request[..length]
                .windows(4)
                .any(|bytes| bytes == b"\r\n\r\n")
            {
                stream
                    .set_read_timeout(Some(
                        deadline.checked_duration_since(Instant::now()).unwrap(),
                    ))
                    .unwrap();
                assert!(
                    length < request.len(),
                    "supported-version request exceeds its bound"
                );
                let read = stream.read(&mut request[length..]).unwrap();
                assert!(read > 0, "supported-version request ended before headers");
                length += read;
            }
            assert!(request[..length].starts_with(request_line));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            requested
                .send(())
                .expect("supported-version observer dropped");
            released
                .recv_timeout(Duration::from_secs(8))
                .expect("supported-version body was not released");
            match stream.write_all(&body) {
                Ok(()) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    false
                }
                Err(error) => panic!("supported-version body write failed: {error}"),
            }
        });
        let service = Arc::new(service);
        let mut caller = None;
        let mut release = Some(release);
        let mut view = None;
        let mut held_shutdown = None;
        let mut provider_cache_before = None;
        let begun_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let journey = std::panic::AssertUnwindSafe(async {
            let pin = library.admit().unwrap();
            let operation = pin.managed_library().unwrap();
            operation.prepare_layout().unwrap();
            if hold_manifest {
                loaders::persist_loader_supported_versions_cache_fixture_for_test(
                    &operation,
                    LoaderComponentId::Fabric,
                    &raw,
                    begun_at_ms,
                )
                .unwrap();
                provider_cache_before = Some(std::fs::read(&cache).unwrap());
            } else {
                axial_minecraft::manifest::persist_version_manifest_cache_fixture_for_test(
                    &operation, &manifest,
                )
                .unwrap();
                assert_eq!(
                    axial_minecraft::manifest::read_cached_manifest_bytes(&operation).unwrap(),
                    (manifest.clone(), true)
                );
            }
            drop(operation);
            drop(pin);
            std::fs::write(&canary, b"preserve supported-version canary\n").unwrap();
            assert!(
                absent(missing_cache) && service.instances.registry().list().unwrap().is_empty()
            );
            assert!(service.instances.pending().unwrap().is_empty());
            caller = Some(tokio::spawn({
                let service = service.clone();
                async move {
                    service
                        .create_view(Some(LoaderComponentId::Fabric.as_str()))
                        .await
                }
            }));
            let observed = tokio::time::timeout(Duration::from_secs(5), observed).await;
            if !matches!(observed, Ok(Ok(()))) && caller.as_ref().unwrap().is_finished() {
                let result = tokio::time::timeout(Duration::from_secs(1), caller.as_mut().unwrap())
                    .await
                    .map(|joined| joined.map(|result| result.map(|_| ())));
                if result.is_ok() {
                    drop(caller.take());
                }
                panic!("create view finished before supported-version request: {result:?}");
            }
            observed
                .expect("supported-version request was not observed")
                .expect("supported-version observer closed");
            let accepted = owner.status();
            assert!(!caller.as_ref().unwrap().is_finished() && !server.is_finished());
            assert_eq!(accepted.running.len(), 1);
            assert!(!accepted.closing && accepted.unsettled.is_empty());
            if cancel_fetch {
                caller.as_ref().unwrap().abort();
                let joined = tokio::time::timeout(Duration::from_secs(1), caller.as_mut().unwrap())
                    .await
                    .expect("disposable caller did not join");
                drop(caller.take());
                let after_drop = owner.status();
                let shutdown = owner.shutdown(Duration::from_secs(2)).await;
                let receipt = owner.shutdown_receipt();
                held_shutdown = Some((
                    joined.is_err_and(|error| error.is_cancelled()),
                    after_drop == accepted,
                    shutdown.is_ok(),
                    receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.belongs_to(&owner)),
                    !server.is_finished(),
                    absent(missing_cache),
                ));
            } else {
                release.take().unwrap().send(()).unwrap();
                let joined = tokio::time::timeout(Duration::from_secs(5), caller.as_mut().unwrap())
                    .await
                    .expect("create view did not settle");
                drop(caller.take());
                view = Some(serde_json::to_value(joined.unwrap().unwrap()).unwrap());
            }
            assert_eq!(service.installs.snapshot(), queue_before);
        })
        .catch_unwind()
        .await;
        let released = release.take().map(|release| release.send(()));
        service.installs.close_admission();
        let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(15)))
            .catch_unwind()
            .await;
        let caller_joined = match caller.take() {
            Some(mut caller) => {
                caller.abort();
                tokio::time::timeout(Duration::from_secs(1), &mut caller)
                    .await
                    .is_ok_and(|joined| match joined {
                        Ok(_) => true,
                        Err(error) => error.is_cancelled(),
                    })
            }
            None => true,
        };
        let observers =
            tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
        let server = tokio::time::timeout(
            Duration::from_secs(18),
            tokio::task::spawn_blocking(move || server.join()),
        )
        .await;
        let final_state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let pin = library.admit().unwrap();
            let operation = pin.managed_library().unwrap();
            (
                service.instances.registry().list().unwrap(),
                service.instances.pending().unwrap(),
                service.installs.snapshot(),
                axial_minecraft::manifest::read_cached_manifest_bytes(&operation),
            )
        }));
        let receipt = owner.shutdown_receipt();
        let receipted = receipt
            .as_ref()
            .is_some_and(|receipt| receipt.belongs_to(&owner));
        let idle = owner.status().is_idle();
        let unsettled = service.installs.has_unsettled_effects();
        let runtime_settled = service.installs.runtime_cache().settle();
        drop(receipt);
        drop(service);
        drop(owner);
        let pins = library.wait_for_pins(Duration::from_secs(2)).await;
        let preserved = if matches!(&shutdown, Ok(Ok(())))
            && matches!(&observers, Ok(Ok(())))
            && matches!(&server, Ok(Ok(Ok(_))))
            && receipted
            && idle
            && !unsettled
            && runtime_settled.is_ok()
            && pins.is_ok()
        {
            Some(library.try_preserve())
        } else {
            None
        };
        let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                matches!(shutdown, Ok(Ok(()))) && caller_joined,
                "supported-version owner/caller did not join"
            );
            assert!(
                matches!(observers, Ok(Ok(()))),
                "queue observers did not join"
            );
            assert!(
                matches!(server, Ok(Ok(Ok(_)))),
                "supported-version server did not join"
            );
            assert!(
                released.is_none_or(|result| result.is_ok()),
                "body gate ended before cleanup"
            );
            assert!(receipted && idle && !unsettled && runtime_settled.is_ok() && pins.is_ok());
            assert!(
                matches!(preserved, Some(Ok(()))),
                "supported-version root did not preserve"
            );
            let (records, pending, queue, cached_manifest) = final_state.unwrap();
            assert!(records.is_empty() && pending.is_empty());
            assert!(
                queue.items.is_empty() && queue.active.is_none() && queue.latest_failure.is_none()
            );
            assert_eq!(queue.registry_revision, queue_before.registry_revision);
            if let Some(provider_cache_before) = provider_cache_before {
                assert_eq!(std::fs::read(&cache).unwrap(), provider_cache_before);
            } else {
                assert_eq!(cached_manifest.as_ref().unwrap(), &(manifest.clone(), true));
                assert_eq!(std::fs::read(&manifest_cache).unwrap(), manifest);
            }
            assert_eq!(
                std::fs::read(&canary).unwrap(),
                b"preserve supported-version canary\n"
            );
            assert!(absent(&root.path().join("instances")));
            assert!(
                std::fs::read_dir(root.path().join("versions"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            if let Err(panic) = journey {
                std::panic::resume_unwind(panic);
            }
            if let Some((
                caller_cancelled,
                retained,
                shutdown,
                receipted,
                body_held,
                cache_absent,
            )) = held_shutdown
            {
                assert!(
                    caller_cancelled && retained,
                    "dropping the caller revoked accepted work"
                );
                assert!(body_held, "supported-version body escaped its gate");
                assert!(
                    shutdown && receipted,
                    "create catalog did not settle on owner cancellation while its body remained withheld (hold_manifest={hold_manifest})"
                );
                assert!(
                    cache_absent && absent(missing_cache),
                    "cancelled create catalog fetch published a cache (hold_manifest={hold_manifest})"
                );
            } else {
                assert!(
                    matches!(server, Ok(Ok(Ok(true)))),
                    "positive body was not delivered"
                );
                let view = view.unwrap();
                assert_eq!(view["defaults"]["source_id"], "net.fabricmc.fabric-loader");
                assert_eq!(view["notices"], serde_json::json!([]));
                assert_eq!(
                    view["versions"],
                    serde_json::json!([
                        {"source_id":"net.fabricmc.fabric-loader", "selection_id":"loader_auto|net.fabricmc.fabric-loader|25w01a", "minecraft_version_id":"25w01a", "display_name":"25w01a", "hint":"~ 1.21.4", "channel":"preview", "download_state":"none", "create_enabled":true, "disabled_reason":null},
                        {"source_id":"net.fabricmc.fabric-loader", "selection_id":"loader_auto|net.fabricmc.fabric-loader|1.21.4", "minecraft_version_id":"1.21.4", "display_name":"1.21.4", "hint":null, "channel":"stable", "download_state":"none", "create_enabled":true, "disabled_reason":null}
                    ])
                );
                assert_eq!(
                    view["channels"],
                    serde_json::json!([
                        {"id":"stable","label":"Stable","enabled":true}, {"id":"preview","label":"Preview","enabled":true},
                        {"id":"experimental","label":"Experimental","enabled":true}, {"id":"legacy","label":"Legacy","enabled":true},
                        {"id":"unknown","label":"Other","enabled":true}
                    ])
                );
                let mut bytes = Vec::new();
                std::fs::File::open(&cache)
                    .unwrap()
                    .take(16 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .unwrap();
                assert!(bytes.len() <= 16 * 1024);
                let cached: CachedCatalog<Vec<axial_minecraft::LoaderGameVersion>> =
                    serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    cached.schema_version,
                    loaders::LOADER_CATALOG_SCHEMA_VERSION
                );
                let finished_at_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                assert!((begun_at_ms..=finished_at_ms).contains(&cached.fetched_at_ms));
                assert_eq!(
                    cached.value, raw,
                    "offline provider list must remain raw rather than cache manifest enrichment"
                );
            }
            if hold_manifest {
                if cancel_fetch {
                    assert!(cached_manifest.is_err() && absent(&manifest_cache));
                } else {
                    assert_eq!(cached_manifest.unwrap(), (manifest.clone(), true));
                    assert_eq!(std::fs::read(&manifest_cache).unwrap(), manifest);
                }
            }
        }));
        if let Err(panic) = verification {
            eprintln!(
                "Retained create catalog cancellation fixture (hold_manifest={hold_manifest}, cancel_fetch={cancel_fetch}): {}",
                root.keep().display()
            );
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn resolution_retains_its_generation_across_managed_reselection() {
    use crate::library::{AdmissionState, LibraryId, LibraryMode};
    use futures_util::FutureExt;
    use std::time::Duration;

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let library_id = LibraryId::new();
    let (mut service, _) = open_fixture(root.path(), library_id);
    let library = service.instances.directories().library().clone();
    let owner = service.instances.tasks.clone();
    let manifest = crate::catalog::tests::manifest(&[("1.21.11", "release")]);
    let (requested, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (catalog, server) =
        crate::catalog::tests::fixture_catalog(manifest.clone(), Some((requested, released)));
    service.catalog = Arc::new(catalog);
    let service = Arc::new(service);
    let mut release = Some(release);
    let mut caller = None;
    let canary = root.path().join("unrelated-user-file.txt");
    let journey = std::panic::AssertUnwindSafe(async {
        let pin = library.admit().unwrap();
        let original_generation = pin.generation();
        let original_path = pin.read_projection().unwrap();
        let operation = pin.managed_library().unwrap();
        operation.prepare_layout().unwrap();
        drop(operation);
        drop(pin);
        let original = library.snapshot();
        std::fs::write(&canary, b"preserve unrelated user bytes\n").unwrap();
        let absent = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        };
        let initially_absent = absent(&root.path().join("cache/version_manifest_v2.json"))
            && absent(&root.path().join("versions/1.21.11"))
            && absent(&root.path().join("instances"));
        let instances_before = service.instances.registry().list().unwrap();
        let pending_before = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let queue_before = service.installs.snapshot();
        caller = Some(tokio::spawn({
            let service = service.clone();
            async move { service.resolve("vanilla|1.21.11").await }
        }));
        tokio::time::timeout(Duration::from_secs(5), observed)
            .await
            .expect("catalog request was not observed")
            .expect("catalog request observer closed");
        let held = !caller.as_ref().unwrap().is_finished();
        let accepted = owner.status();
        let mut change = library.begin_switch().unwrap();
        let changing = library.snapshot();
        let second_refused =
            tokio::time::timeout(Duration::from_secs(1), service.resolve("vanilla|1.21.11"))
                .await
                .is_ok_and(|result| matches!(result, Err(InstanceError::LibraryUnavailable)));
        let after_refusal = owner.status();
        // The persisted managed selection already names this same root and identity.
        change.prepare_managed(library_id).unwrap();
        let committed_generation = change.commit_after_persistence().unwrap();
        let current = library.admit().unwrap();
        let current_generation = current.generation();
        let current_library = current.library_id();
        let current_path = current.read_projection().unwrap();
        drop(current);
        let retained_during_fetch = !library.collect_retired();
        let still_held = !caller.as_ref().unwrap().is_finished() && !server.is_finished();
        release.take().unwrap().send(()).unwrap();
        let joined = tokio::time::timeout(Duration::from_secs(5), caller.as_mut().unwrap())
            .await
            .expect("original resolution did not settle");
        drop(caller.take());
        let (target, request, admission) = joined.unwrap().unwrap();
        let returned_generation = admission.generation().generation();
        let returned_library = admission.generation().library_id();
        let original_valid = admission.validate();
        let operation = admission.generation().managed_library().unwrap();
        let cached = axial_minecraft::manifest::read_cached_manifest_bytes(&operation).unwrap();
        drop(operation);
        let finished_owner = owner.status();
        let retained_by_admission = !library.collect_retired();
        let retained = library.snapshot();
        drop(admission);
        let retired = library.collect_retired();
        let after_drop = library.snapshot();
        let instances_after = service.instances.registry().list().unwrap();
        let pending_after = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let queue_after = service.installs.snapshot();
        let unpublished =
            absent(&root.path().join("versions/1.21.11")) && absent(&root.path().join("instances"));
        move || {
            assert!(initially_absent && held && still_held && second_refused);
            assert_eq!(original.admission, AdmissionState::Open);
            assert!(original.retiring.is_empty());
            let original = original.current.unwrap();
            assert_eq!(original.mode, LibraryMode::Managed);
            assert_eq!(original.generation, original_generation);
            assert_eq!(original.library_id, library_id);
            assert_eq!(original.pins, 0);
            assert_eq!(changing.admission, AdmissionState::Changing);
            assert_eq!(accepted.running.len(), 1);
            assert!(accepted.unsettled.is_empty() && !accepted.closing);
            assert_eq!(
                after_refusal, accepted,
                "new resolution must refuse before provider work"
            );
            assert_ne!(committed_generation, original_generation);
            assert_eq!(current_generation, committed_generation);
            assert_eq!(current_library, library_id);
            assert_eq!(current_path, original_path);
            assert_eq!(returned_generation, original_generation);
            assert_eq!(returned_library, library_id);
            assert_eq!(target.version_id(), "1.21.11");
            assert_eq!(
                request,
                InstallQueueRequest::Vanilla {
                    version_id: "1.21.11".into()
                }
            );
            assert!(original_valid.is_ok());
            assert_eq!(cached, (manifest, true));
            assert!(finished_owner.is_idle() && !finished_owner.closing);
            assert!(retained_during_fetch && retained_by_admission);
            assert_eq!(retained.retiring.len(), 1);
            assert_eq!(retained.retiring[0].generation, original_generation);
            assert!(retained.retiring[0].pins > 0);
            assert!(retired && after_drop.retiring.is_empty());
            assert_eq!(after_drop.current.unwrap().generation, committed_generation);
            assert!(instances_before.is_empty() && unpublished);
            assert_eq!(instances_after, instances_before);
            assert_eq!(pending_before, serde_json::json!([]));
            assert_eq!(pending_after, pending_before);
            assert_eq!(queue_after, queue_before);
        }
    })
    .catch_unwind()
    .await;
    if let Some(release) = release.take() {
        let _ = release.send(());
    }
    service.installs.close_admission();
    let shutdown = std::panic::AssertUnwindSafe(owner.shutdown(Duration::from_secs(5)))
        .catch_unwind()
        .await;
    let remaining = match caller.take() {
        Some(mut caller) => {
            let joined = tokio::time::timeout(Duration::from_secs(5), &mut caller).await;
            if joined.is_err() {
                caller.abort();
                let _ = tokio::time::timeout(Duration::from_secs(1), caller).await;
            }
            Some(joined.is_ok_and(|result| result.is_ok()))
        }
        None => None,
    };
    let observers =
        tokio::time::timeout(Duration::from_secs(5), service.installs.join_observers()).await;
    let server = tokio::task::spawn_blocking(move || server.join());
    let server = tokio::time::timeout(Duration::from_secs(16), server).await;
    let receipt = owner.shutdown_receipt();
    let receipted = receipt
        .as_ref()
        .is_some_and(|receipt| receipt.belongs_to(&owner));
    let idle = owner.status().is_idle();
    let unsettled = service.installs.has_unsettled_effects();
    let runtime_settled = service.installs.runtime_cache().settle();
    drop(receipt);
    drop(service);
    drop(owner);
    let pins = library.wait_for_pins(Duration::from_secs(2)).await;
    let preserved = if matches!(&shutdown, Ok(Ok(())))
        && matches!(&observers, Ok(Ok(())))
        && matches!(&server, Ok(Ok(Ok(()))))
        && receipted
        && idle
        && !unsettled
        && runtime_settled.is_ok()
        && pins.is_ok()
    {
        Some(library.try_preserve())
    } else {
        None
    };
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(matches!(shutdown, Ok(Ok(()))), "task owner did not join");
        if let Some(joined) = remaining {
            assert!(joined, "resolution caller did not join");
        }
        assert!(
            matches!(observers, Ok(Ok(()))),
            "queue observers did not join"
        );
        assert!(
            matches!(server, Ok(Ok(Ok(())))),
            "catalog server did not join"
        );
        assert!(receipted && idle && !unsettled && runtime_settled.is_ok());
        assert!(pins.is_ok(), "profile capabilities did not drain");
        assert!(
            matches!(preserved, Some(Ok(()))),
            "profile root did not preserve"
        );
        assert_eq!(
            std::fs::read(canary).unwrap(),
            b"preserve unrelated user bytes\n"
        );
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained generation-resolution fixture: {}",
            root.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn creation_rejects_a_restored_library_with_an_old_admission() {
    use futures_util::FutureExt;
    use std::time::Duration;

    let (root, service, _) = fixture();
    let service = Arc::new(service);
    let mut waiter = None;
    let journey = std::panic::AssertUnwindSafe(async {
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        let pin = service.instances.directories().library().admit().unwrap();
        let receipt = service
            .installs
            .ready_version(&pin, "1.21.4")
            .await
            .unwrap();
        let initially_valid = receipt.revalidate();
        let protected: Vec<_> = ["json", "jar"]
            .into_iter()
            .map(|extension| {
                let path = root
                    .path()
                    .join(format!("versions/1.21.4/1.21.4.{extension}"));
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        let request = CreateInstanceRequest {
            name: "Restored admission".into(),
            selection_id: "vanilla|1.21.4".into(),
            ..Default::default()
        };
        let (target, _, admission) = service.resolve(&request.selection_id).await.unwrap();
        let target_version = target.version_id().to_owned();
        let versions = pin
            .directory()
            .unwrap()
            .open_directory(&axial_fs::LeafName::new("versions").unwrap())
            .unwrap();
        let revision_before = versions.revision().unwrap();
        let observe_changed_revision = || async {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let revision = versions.revision().unwrap();
                    if revision != revision_before {
                        break revision;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
        };
        let queue_before = service.installs.snapshot();
        let instances_before = service.instances.registry().list().unwrap();
        let pending_before = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let instance_parent = root.path().join("instances");
        let namespace_absent_before = std::fs::symlink_metadata(&instance_parent)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        // An external entry changes the library scan, not the selected installation.
        let external = root.path().join("versions/external-degraded-entry");
        let absent_before = std::fs::symlink_metadata(&external)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        std::fs::create_dir(&external).unwrap();
        let metadata = external.join("external-degraded-entry.json");
        std::fs::write(&metadata, b"{not valid external version metadata\n").unwrap();
        let changed_revision = observe_changed_revision().await;
        let selected_after_change = receipt.revalidate();
        std::fs::remove_file(&metadata).unwrap();
        std::fs::remove_dir(&external).unwrap();
        let restored_revision = observe_changed_revision().await;
        let selected_after_restore = receipt.revalidate();
        let restored_bytes: Vec<_> = protected
            .iter()
            .map(|(path, _)| std::fs::read(path).unwrap())
            .collect();
        let work = service
            .instances
            .create(request.clone(), target, admission)
            .unwrap();
        waiter = Some(tokio::spawn(async move { work.join().await }));
        let old_result = tokio::time::timeout(Duration::from_secs(5), waiter.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(waiter.take());
        let queue_after = service.installs.snapshot();
        let instances_after = service.instances.registry().list().unwrap();
        let pending_after = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let namespace_absent_after = std::fs::symlink_metadata(&instance_parent)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let fresh_result = if matches!(&old_result, Err(InstanceError::VersionUnavailable)) {
            let (target, _, admission) = service.resolve(&request.selection_id).await.unwrap();
            let work = service
                .instances
                .create(request, target, admission)
                .unwrap();
            waiter = Some(tokio::spawn(async move { work.join().await }));
            let result = tokio::time::timeout(Duration::from_secs(5), waiter.as_mut().unwrap())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            drop(waiter.take());
            Some(result)
        } else {
            None
        };
        let fresh_instances = service.instances.registry().list().unwrap();
        let fresh_pending = serde_json::to_value(service.instances.pending().unwrap()).unwrap();
        let fresh_queue = service.installs.snapshot();
        let selected_final = receipt.revalidate();
        drop(receipt);
        drop(versions);
        drop(pin);
        move || {
            assert!(absent_before && namespace_absent_before);
            assert!(
                std::fs::symlink_metadata(external)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
            assert_ne!(changed_revision.unwrap(), revision_before);
            assert_ne!(restored_revision.unwrap(), revision_before);
            assert_eq!(target_version, "1.21.4");
            for valid in [
                initially_valid,
                selected_after_change,
                selected_after_restore,
                selected_final,
            ] {
                assert!(
                    valid.is_ok(),
                    "selected installation lost its independent proof"
                );
            }
            for ((path, before), restored) in protected.into_iter().zip(restored_bytes) {
                assert_eq!(restored, before);
                assert_eq!(std::fs::read(path).unwrap(), before);
            }
            assert!(
                matches!(old_result, Err(InstanceError::VersionUnavailable)),
                "the original admission did not refuse the changed library"
            );
            assert_eq!(instances_after, instances_before);
            assert!(instances_before.is_empty());
            assert_eq!(pending_after, pending_before);
            assert_eq!(pending_before, serde_json::json!([]));
            assert_eq!(queue_after, queue_before);
            assert!(
                namespace_absent_after,
                "stale admission published an instance namespace"
            );
            let fresh = fresh_result
                .expect("fresh creation follows the original refusal")
                .unwrap();
            assert_eq!(fresh.name, "Restored admission");
            assert_eq!(fresh.version_id, "1.21.4");
            assert_eq!(fresh_instances.len(), 1);
            assert_eq!(fresh_instances[0].instance.id, fresh.id);
            assert_eq!(fresh_pending, serde_json::json!([]));
            assert_eq!(fresh_queue, queue_before);
        }
    })
    .catch_unwind()
    .await;
    let shutdown =
        std::panic::AssertUnwindSafe(service.instances.tasks.shutdown(Duration::from_secs(5)))
            .catch_unwind()
            .await;
    let remaining = match waiter.take() {
        Some(mut waiter) => {
            let result = tokio::time::timeout(Duration::from_secs(5), &mut waiter).await;
            if result.is_err() {
                waiter.abort();
                let _ = tokio::time::timeout(Duration::from_secs(1), waiter).await;
            }
            Some(result)
        }
        None => None,
    };
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(matches!(shutdown, Ok(Ok(()))), "task owner did not join");
        if let Some(result) = remaining {
            assert!(
                matches!(result, Ok(Ok(Ok(_)))),
                "creation waiter did not join"
            );
        }
        assert!(service.instances.tasks.status().is_idle());
        match journey {
            Ok(verify) => verify(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }));
    if let Err(panic) = verification {
        eprintln!(
            "Retained restored-admission fixture: {}",
            root.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn second_instance_reuses_exact_verified_install_without_requeueing() {
    let (_root, service, _) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let before = service.installs.snapshot();
    let service = Arc::new(service);
    let first = service
        .create(super::super::create::tests::request("First ready instance"))
        .await
        .unwrap();
    let second = service
        .create(super::super::create::tests::request(
            "Second ready instance",
        ))
        .await
        .unwrap();
    assert_ne!(first.instance.instance.id, second.instance.instance.id);
    assert!(first.install_queue.is_none());
    assert!(second.install_queue.is_none());
    assert_eq!(second.view_model.summary, "Instance created.");
    assert_eq!(service.installs.snapshot(), before);
    assert_eq!(service.instances.registry().list().unwrap().len(), 2);
}

#[tokio::test]
async fn scanner_ready_artifact_drift_offers_install_without_masking_transient_blocks() {
    let (root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    select_unowned_microsoft_account(&accounts);
    let account_block = service.enrich(instance.clone(), &versions).await;
    assert_eq!(account_block.launch_action.primary_action, "blocked");
    assert!(account_block.status_detail.contains("account"));
    accounts.create_offline_account("FixturePlayer").unwrap();
    let pin = service.instances.directories().library().admit().unwrap();
    let operation = pin.managed_library().unwrap();
    let publication =
        axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
    let busy = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(busy.error.unwrap().code, LaunchError::InstanceBusy);
    let blocked = service.enrich(instance.clone(), &versions).await;
    assert_eq!(blocked.launch_action.primary_action, "blocked");
    assert_eq!(blocked.launch_action.label, "Unavailable");
    assert!(blocked.needs_install.is_empty());
    drop(publication);
    let runtime_block = service.enrich(instance.clone(), &versions).await;
    assert_eq!(runtime_block.launch_action.primary_action, "blocked");
    assert_eq!(
        runtime_block.status_detail,
        "The selected Java executable is missing."
    );
    std::fs::write(
        root.path().join("versions/1.21.4/1.21.4.jar"),
        b"externally changed",
    )
    .unwrap();
    let scanned = service.installed().await.unwrap();
    assert!(
        scanned
            .iter()
            .any(|entry| entry.id == "1.21.4" && entry.launchable)
    );
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        preflight.error.unwrap().code,
        LaunchError::InstallUnavailable
    );
    let drifted = service.enrich(instance, &scanned).await;
    assert_eq!(drifted.launch_action.primary_action, "install");
    assert_eq!(drifted.launch_action.label, "Install");
    assert_eq!(drifted.needs_install, "1.21.4");
    assert!(!drifted.launchable);
    assert!(drifted.install_target.is_some());
}

#[cfg(unix)]
#[test]
fn list_summary_leaves_content_integrity_to_strict_preflight() {
    use futures_util::FutureExt;
    use std::os::unix::fs::PermissionsExt;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (root, service, accounts) = runtime.block_on(async { fixture() });
    let outcome = runtime.block_on(std::panic::AssertUnwindSafe(async {
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    accounts.create_offline_account("SummaryPlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(&java, format!(
        "#!/bin/sh\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
        std::env::consts::ARCH,
    )).unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": service.settings.current().unwrap().revision,
                "java_path_override": java.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let instance = super::super::create::tests::create(&service.instances, "Summary").await;
    let healthy = service.list().await;
    let strict_healthy = service.launch.preflight(instance.id.clone()).await;
    let client = root.path().join("versions/1.21.4/1.21.4.jar");
    let mut observations = Vec::new();
    for (case, change) in [
        ("same-size client", 0),
        ("missing client", 1),
        ("client size drift", 2),
    ] {
        let original = std::fs::read(&client).unwrap();
        match change {
            0 => std::fs::write(&client, vec![b'x'; original.len()]).unwrap(),
            1 => std::fs::remove_file(&client).unwrap(),
            _ => std::fs::write(&client, b"different size").unwrap(),
        }
        let rows = service.list().await;
        let strict = service.launch.preflight(instance.id.clone()).await;
        std::fs::write(&client, original).unwrap();
        observations.push((case, rows, strict, change == 0));
    }
    let strict_restored = service.launch.preflight(instance.id.clone()).await;
    let healthy = healthy.unwrap();
    assert_eq!(healthy.instances.len(), 1);
    assert!(healthy.instances[0].launchable);
    assert!(strict_healthy.launchable, "{strict_healthy:?}");
    assert!(strict_restored.launchable, "{strict_restored:?}");
    for (case, rows, strict, summary_ready) in observations {
        let rows = rows.unwrap();
        assert_eq!(rows.instances.len(), 1, "{case}");
        let row = &rows.instances[0];
        assert_eq!(row.instance.id, instance.id, "{case}");
        assert_eq!(row.launchable, summary_ready, "{case}: {row:?}");
        assert_eq!(
            row.launch_action.primary_action,
            if summary_ready { "launch" } else { "install" },
            "{case}"
        );
        assert!(!strict.launchable, "{case}: {strict:?}");
        assert_eq!(
            strict.error.unwrap().code,
            LaunchError::InstallUnavailable,
            "{case}"
        );
    }
    }).catch_unwind());
    let stopped = runtime.block_on(
        service
            .instances
            .tasks
            .shutdown(std::time::Duration::from_secs(5)),
    );
    if outcome.is_err() || stopped.is_err() {
        let retained = root.keep();
        if stopped.is_err() {
            std::mem::forget((service, accounts, runtime));
        }
        eprintln!("summary fixture retained: {}", retained.display());
    }
    stopped.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn unready_catalogs(versions: &[VersionEntry]) -> [Vec<VersionEntry>; 3] {
    let mut not_installed = versions.to_vec();
    for version in &mut not_installed {
        version.installed = false;
    }
    let mut not_launchable = versions.to_vec();
    for version in &mut not_launchable {
        version.launchable = false;
    }
    [Vec::new(), not_installed, not_launchable]
}

#[tokio::test]
async fn absent_or_unready_catalog_does_not_turn_launch_blocks_into_install_permission() {
    let (_root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    assert!(
        versions
            .iter()
            .any(|version| version.id == instance.version_id)
    );

    select_unowned_microsoft_account(&accounts);
    let unowned_account = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        unowned_account.error.unwrap().code,
        LaunchError::OnlineAccountUnavailable
    );
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.status_detail.contains("account"));
        assert!(blocked.needs_install.is_empty());
        assert!(blocked.install_target.is_some());
    }

    accounts.create_offline_account("FixturePlayer").unwrap();
    let pin = service.instances.directories().library().admit().unwrap();
    let operation = pin.managed_library().unwrap();
    let publication =
        axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
    let busy = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(busy.error.unwrap().code, LaunchError::InstanceBusy);
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert_eq!(blocked.status_detail, LaunchError::InstanceBusy.to_string());
        assert!(blocked.needs_install.is_empty());
    }
    drop(publication);

    let runtime = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        serde_json::to_value(runtime.error.unwrap()).unwrap()["code"],
        "runtime_unavailable"
    );
    for catalog in unready_catalogs(&versions) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(
            blocked.status_detail,
            "The selected Java executable is missing."
        );
        assert!(blocked.needs_install.is_empty());
    }
}

#[tokio::test]
async fn guarded_missing_install_evidence_offers_install_without_a_selected_account() {
    for drifted in [false, true] {
        let (root, service, _accounts) = fixture();
        let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        let versions = service.installed().await.unwrap();
        if drifted {
            std::fs::write(
                root.path().join("versions/1.21.4/1.21.4.jar"),
                b"externally changed",
            )
            .unwrap();
        } else {
            service
                .instances
                .registry()
                .storage()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "DELETE FROM installed_versions WHERE version_id = '1.21.4'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let preflight = service.launch.preflight(instance.id.clone()).await;
        assert_eq!(
            preflight.error.unwrap().code,
            LaunchError::InstallUnavailable
        );
        for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
            let missing = service.enrich(instance.clone(), &catalog).await;
            assert_eq!(missing.launch_action.primary_action, "install");
            assert_eq!(missing.launch_action.label, "Install");
            assert_eq!(missing.needs_install, instance.version_id);
            assert!(!missing.launchable);
            assert_eq!(
                missing.install_target.unwrap().version_id,
                instance.version_id
            );
        }
    }
}

#[tokio::test]
async fn unavailable_install_metadata_does_not_offer_reinstall() {
    let (_root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    accounts.create_offline_account("FixturePlayer").unwrap();
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TABLE installed_versions")?;
            Ok(())
        })
        .unwrap();
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        preflight.error.unwrap().code,
        LaunchError::LibraryUnavailable
    );
    for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.needs_install.is_empty());
    }
}

#[tokio::test]
async fn unavailable_setup_metadata_does_not_fabricate_resume_or_install_permission() {
    let (_root, service, _accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DELETE FROM installed_versions")?;
            Ok(())
        })
        .unwrap();
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(
        preflight.error.unwrap().code,
        LaunchError::InstallUnavailable
    );
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TABLE instance_setups")?;
            Ok(())
        })
        .unwrap();
    assert!(has_pending(service.instances.registry().storage(), &instance.id).is_err());
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert!(!preflight.launchable);
    assert_eq!(preflight.error.unwrap().code, LaunchError::InstanceChanged);
    for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert!(!blocked.launchable);
        assert!(!blocked.launch_action.launchable);
        assert_eq!(blocked.launch_action.state_id, "blocked");
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.needs_install.is_empty());
        assert_eq!(
            blocked.status_detail,
            "Instance setup status could not be read. Refresh and try again."
        );
        assert_eq!(
            blocked.launch_action.disabled_reason.as_deref(),
            Some(blocked.status_detail.as_str())
        );
    }
}

#[cfg(unix)]
async fn accepted_setup_fixture() -> (tempfile::TempDir, SetupService, SetupWork) {
    use crate::content::provenance::{ContentManifest, MANIFEST_FILE, ManifestEntry};
    use std::os::unix::fs::PermissionsExt;

    let (root, mut service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    accounts.create_offline_account("FixturePlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(
        &java,
        format!(
            "#!/bin/sh\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": service.settings.current().unwrap().revision,
                "java_path_override": java.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let client = ProviderClient::new(ClientConfig::default()).unwrap();
    let content = Arc::new(ContentService::new(client.clone()).unwrap());
    let mutations = Arc::new(ContentMutations::new(
        service.instances.directories().clone(),
        client,
        service.instances.tasks.clone(),
    ));
    let work = super::tests::pending_content_work(
        service.instances.clone(),
        content.clone(),
        mutations.clone(),
    )
    .await;
    let directory = work.instance().game_directory().read_projection().unwrap();
    let StoredSetupIntent::Content(stored) = &work.stored else {
        unreachable!()
    };
    let mut manifest = ContentManifest::default();
    for file in stored.artifacts.as_ref().unwrap() {
        manifest
            .try_upsert(
                ManifestEntry::managed(
                    file.canonical_id.clone(),
                    file.provider,
                    file.project_id.clone(),
                    file.version_id.clone(),
                    file.kind,
                    &file.file,
                    file.dependencies.clone(),
                    file.title.clone(),
                )
                .unwrap(),
            )
            .unwrap();
        std::fs::write(
            directory.join("resourcepacks").join(&file.file.filename),
            b"fixture",
        )
        .unwrap();
    }
    std::fs::write(
        directory.join(MANIFEST_FILE),
        manifest.encode_managed().unwrap(),
    )
    .unwrap();
    service.installs = Arc::new(
        service
            .installs
            .as_ref()
            .clone()
            .with_content(content.clone(), mutations.clone()),
    );
    (root, service.with_content(content, mutations), work)
}

#[cfg(unix)]
#[tokio::test]
async fn resumed_setup_response_is_ready_after_accepted_admission_releases() {
    let (_root, mut service, work) = accepted_setup_fixture().await;
    let instance = work.instance().record().instance.clone();
    let (restored, queue_id) =
        crate::install::queue::tests::interrupted_setup_fixture(&service.installs, &work).await;
    drop(work);
    let (content, mutations) = service.content.as_ref().unwrap();
    service.installs = Arc::new(restored.with_content(content.clone(), mutations.clone()));
    let service = Arc::new(service);
    let response = service.resume_setup(&instance.id).await.unwrap();
    assert!(!has_pending(service.instances.registry().storage(), &instance.id).unwrap());
    assert_eq!(
        service.installs.status(&queue_id).unwrap().outcome,
        Some(crate::install::model::InstallOutcome::Succeeded)
    );
    assert_eq!(
        response
            .install_queue
            .unwrap()
            .started_install
            .unwrap()
            .install_id,
        queue_id
    );
    let current = service
        .enrich(instance, &service.installed().await.unwrap())
        .await;
    assert!(current.launchable, "{}", current.status_detail);
    assert!(
        response.instance.launchable,
        "{}",
        response.instance.status_detail
    );
    assert_eq!(response.instance.launch_action.primary_action, "launch");
}

#[tokio::test]
async fn detached_setup_resumes_same_intent_after_ready_content_rollback() {
    detached_setup_ready_resume(false).await;
}

#[tokio::test]
async fn detached_setup_retains_intent_when_content_rollback_acknowledgement_is_refused() {
    detached_setup_ready_resume(true).await;
}

async fn detached_setup_ready_resume(refuse_acknowledgement: bool) {
    use crate::{
        content::{model::CanonicalId, packs::PackArchive, provenance::MANIFEST_FILE},
        instances::from_pack::PreparedPackCreation,
        library::LibraryId,
    };
    use std::{io::Write, path::Path, process::Command, time::Duration};

    const ROOT: &str = "AXIAL_SETUP_READY_CRASH_ROOT";
    const LIBRARY: &str = "AXIAL_SETUP_READY_CRASH_LIBRARY";
    const ORIGINAL: &[u8] = b"{\"schema_version\":3,\"entries\":[]}\n";

    fn open(root: &Path, library_id: LibraryId) -> Arc<SetupService> {
        let (mut service, _) = open_fixture(root, library_id);
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let content = Arc::new(ContentService::new(client.clone()).unwrap());
        let mutations = Arc::new(ContentMutations::new(
            service.instances.directories().clone(),
            client,
            service.instances.tasks.clone(),
        ));
        service.installs = Arc::new(
            service
                .installs
                .as_ref()
                .clone()
                .with_content(content.clone(), mutations.clone()),
        );
        Arc::new(service.with_content(content, mutations))
    }

    fn intent(service: &SetupService, id: &InstanceId) -> (String, String, String) {
        service
            .instances
            .registry()
            .storage()
            .read(|db| {
                db.query_row(
                    "SELECT plan_id,request_json,phase FROM instance_setups WHERE instance_id=?1",
                    [id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(StorageError::from)
            })
            .unwrap()
    }

    fn content_receipt(service: &SetupService, id: &InstanceId) -> Option<String> {
        service.instances.registry().storage().read(|db| {
            db.query_row(
                "SELECT receipt_json FROM content_batches WHERE instance_id=?1 AND length(CAST(receipt_json AS BLOB))<=65536",
                [id.as_str()], |row| row.get(0),
            ).optional().map_err(StorageError::from)
        }).unwrap()
    }

    fn public_before(game: &Path) {
        assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), ORIGINAL);
        assert_eq!(
            std::fs::read(game.join("config/user.txt")).unwrap(),
            b"unrelated"
        );
        assert!(!game.join("config/accepted.txt").exists());
    }

    if let Some(root) = std::env::var_os(ROOT) {
        let root = std::path::PathBuf::from(root);
        assert_eq!(root.canonicalize().unwrap(), root);
        let service = open(
            &root,
            LibraryId::parse(&std::env::var(LIBRARY).unwrap()).unwrap(),
        );
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, bytes) in [
            ("modrinth.index.json", br#"{"formatVersion":1,"game":"minecraft","versionId":"fixture-v1","name":"Accepted fixture","dependencies":{"minecraft":"1.21.4"},"files":[]}"#.as_slice()),
            ("overrides/config/accepted.txt", b"accepted payload".as_slice()),
        ] {
            zip.start_file(path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let prepared = PreparedPackCreation::new(
            CanonicalId("modrinth:accepted-fixture".into()),
            "fixture-v1".into(),
            PackArchive::read(zip.finish().unwrap().into_inner()).unwrap(),
            "vanilla|1.21.4",
        )
        .unwrap();
        let stored: StoredSetupIntent = serde_json::from_value(serde_json::json!({
            "kind":"modpack", "canonical_id":"modrinth:accepted-fixture", "version_id":"fixture-v1",
            "selection_id":"vanilla|1.21.4", "installed_version_id":"1.21.4",
            "minecraft_version":"1.21.4", "loader_key":"vanilla",
            "archive_fingerprint":prepared.plan().fingerprint(),
            "install":{"kind":"vanilla", "version_id":"1.21.4"}
        }))
        .unwrap();
        let request_json = serde_json::to_string(&stored).unwrap();
        let admitted = service
            .instances
            .create_admitted(
                CreateInstanceRequest {
                    name: "Detached setup rollback".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: "vanilla|1.21.4".into(),
                    version_id: "1.21.4".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "vanilla".into(),
                },
                service
                    .instances
                    .creation_admission_for_tests()
                    .await
                    .unwrap(),
                SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: request_json.clone(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let id = admitted.record().instance.id.clone();
        let game = root.join("instances").join(id.as_str());
        std::fs::write(game.join(MANIFEST_FILE), ORIGINAL).unwrap();
        std::fs::write(game.join("config/user.txt"), b"unrelated").unwrap();
        let accepted = intent(&service, &id);
        let (content, mutations) = service.content.as_ref().unwrap();
        let work = SetupWork {
            instances: service.instances.clone(),
            content: content.clone(),
            mutations: mutations.clone(),
            admitted,
            stored,
            request_json,
            prepared_pack: Some(prepared),
        };
        let observer = service.clone();
        let progress = Arc::new(move |event: axial_minecraft::DownloadProgress| {
            if event.phase != "content_commit" || event.current != 0 {
                return;
            }
            assert_eq!(event.total, 1);
            assert!(!event.done);
            assert_eq!(intent(&observer, &id), accepted);
            public_before(&game);
            let raw = content_receipt(&observer, &id).unwrap();
            let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(receipt["native_settled"], false);
            assert!(
                receipt["ready_checkpoint"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            );
            assert!(observer.content.as_ref().unwrap().1.has_unsettled_effects());
            std::process::exit(42);
        });
        let result = service
            .instances
            .tasks
            .try_spawn(work.clone(), move |cancel| async move {
                work.execute(&cancel, progress).await
            })
            .unwrap()
            .join()
            .await;
        panic!("accepted setup did not reach its Content Ready boundary: {result:?}");
    }

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let library_id = LibraryId::new();
    let selector = if refuse_acknowledgement {
        "instances::setup::readiness_tests::detached_setup_retains_intent_when_content_rollback_acknowledgement_is_refused"
    } else {
        "instances::setup::readiness_tests::detached_setup_resumes_same_intent_after_ready_content_rollback"
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", selector, "--nocapture"])
        .env(ROOT, root.path())
        .env(LIBRARY, library_id.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let timed_out = child.try_wait().unwrap().is_none();
    if timed_out {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !timed_out && output.status.code() == Some(42),
        "Ready boundary not reached: {:?}; {} {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let service = open(root.path(), library_id);
    let records = service.instances.registry().list().unwrap();
    assert_eq!(records.len(), 1);
    let original = &records[0];
    let id = &original.instance.id;
    let game = root.path().join("instances").join(id.as_str());
    let accepted = intent(&service, id);
    assert_eq!(accepted.2, "pending");
    public_before(&game);
    assert!(!service.installs.has_setup_work(id));
    assert!(
        crate::content::install::has_pending(service.instances.registry().storage(), id).unwrap()
    );
    let original_receipt = content_receipt(&service, id).unwrap();
    let receipt: serde_json::Value = serde_json::from_str(&original_receipt).unwrap();
    let checkpoint = receipt["ready_checkpoint"].as_str().unwrap();
    axial_minecraft::managed_path::ManagedContentStagingCheckpoint::decode(checkpoint).unwrap();
    let checkpoint: serde_json::Value = serde_json::from_str(checkpoint).unwrap();
    let private = game.join(checkpoint["private_name"].as_str().unwrap());
    assert!(
        std::fs::symlink_metadata(&private)
            .unwrap()
            .file_type()
            .is_dir()
    );
    if refuse_acknowledgement {
        let operation = uuid::Uuid::parse_str(receipt["operation_id"].as_str().unwrap()).unwrap();
        service.instances.registry().storage().transaction(|db| {
            db.execute_batch(&format!(
                "CREATE TRIGGER refuse_setup_content_acknowledgement BEFORE DELETE ON content_batches WHEN OLD.instance_id='{id}' AND OLD.operation_id='{operation}' BEGIN SELECT RAISE(ABORT, 'injected Content acknowledgement refusal'); END;"
            )).map_err(StorageError::from)
        }).unwrap();
    }
    let stored: StoredSetupIntent = serde_json::from_str(&accepted.1).unwrap();
    let expected = stored.queue_request(&original.instance);
    let writer = service
        .instances
        .directories()
        .exclusions()
        .try_acquire(
            std::iter::empty::<String>(),
            [crate::install::queue::library_artifact(
                &library_id.to_string(),
            )],
        )
        .unwrap();
    let blocker = service
        .installs
        .enqueue(InstallQueueRequest::Vanilla {
            version_id: "1.20.1".into(),
        })
        .await
        .unwrap()
        .started_install
        .unwrap()
        .install_id;
    let response = tokio::time::timeout(Duration::from_secs(5), service.resume_setup(id)).await;
    let private_removed = matches!(
        std::fs::symlink_metadata(&private),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    let after = intent(&service, id);
    let content_pending =
        crate::content::install::has_pending(service.instances.registry().storage(), id).unwrap();
    let content_unsettled = service.content.as_ref().unwrap().1.has_unsettled_effects();
    let after_receipt = content_receipt(&service, id);
    let content_queue_rows: i64 = service
        .instances
        .registry()
        .storage()
        .read(|db| {
            db.query_row(
                "SELECT COUNT(*) FROM install_queue WHERE request_json=?1",
                [serde_json::to_string(&expected).unwrap()],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
        })
        .unwrap();
    let attached = service.installs.has_setup_work(id);
    let queued = service.installs.snapshot();
    service.installs.close_admission();
    service
        .instances
        .tasks
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), service.installs.join_observers())
        .await
        .unwrap()
        .unwrap();
    let shutdown = service.instances.tasks.shutdown_receipt().unwrap();
    service
        .content
        .as_ref()
        .unwrap()
        .1
        .release_shutdown_admissions(&shutdown)
        .unwrap();
    service.release_shutdown_admissions(&shutdown).unwrap();
    service.installs.shutdown_queued().unwrap();
    drop(writer);
    service
        .instances
        .directories()
        .library()
        .try_preserve()
        .unwrap();

    assert_eq!(after, accepted);
    assert_eq!(
        service.instances.registry().get_record(id).unwrap(),
        *original
    );
    public_before(&game);
    assert!(
        private_removed,
        "Ready rollback must remove its exact private directory before Content acknowledgement"
    );
    assert_eq!(content_receipt(&service, id), after_receipt);
    if refuse_acknowledgement {
        assert!(matches!(
            response.expect("bounded setup resume"),
            Err(InstanceError::SettlementRequired)
        ));
        assert!(content_pending && content_unsettled);
        assert_eq!(after_receipt.as_deref(), Some(original_receipt.as_str()));
        assert!(!attached && queued.active.is_none());
        assert_eq!(content_queue_rows, 0);
        assert_eq!(queued.items.len(), 1);
        assert_eq!(queued.items[0].queue_id, blocker);
        return;
    }
    assert!(
        !content_pending && !content_unsettled,
        "rollback must retire only the Content receipt"
    );
    assert_eq!(content_queue_rows, 1);
    let response = response
        .expect("setup resume must finish within its bound")
        .expect("proven Content rollback must requeue the same accepted setup");
    assert_eq!(response.instance.instance.id, *id);
    assert_eq!(response.view_model.state_id, "setup_queued");
    assert!(attached && queued.active.is_none());
    let started = response.install_queue.unwrap().started_install.unwrap();
    assert_ne!(started.install_id, blocker);
    let item = queued
        .items
        .iter()
        .find(|item| item.queue_id == started.install_id)
        .unwrap();
    let content = item.install_item.content.as_ref().unwrap();
    assert_eq!(
        InstallQueueRequest::Content {
            instance_id: content.instance_id.clone(),
            label: content.label.clone(),
            action: content.action.clone(),
        },
        expected
    );
    let status = service.installs.status(&started.install_id).unwrap();
    assert!(!status.done && status.outcome.is_none());
    assert_eq!(status.view_model.phase_id, "queued");
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_setup_response_still_invalidates_after_accepted_admission_releases() {
    let (_root, service, work) = accepted_setup_fixture().await;
    let instance = work.instance().record().instance.clone();
    let queued_instance = instance.clone();
    let queue = service.installs.clone();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let worker_release = release.clone();
    let (completed, completion) = tokio::sync::oneshot::channel();
    let task = service
        .instances
        .tasks
        .try_spawn(work.clone(), move |cancel| async move {
            work.execute(&cancel, Arc::new(|_| {})).await?;
            completed.send(()).unwrap();
            worker_release.acquire_owned().await.unwrap().forget();
            Ok((queued_instance, queue.snapshot(), setup_queued_result()))
        })
        .unwrap();
    let mut response = Box::pin(service.finish_setup(task));
    assert!(futures_util::poll!(&mut response).is_pending());
    completion.await.unwrap();
    let blocked = service.launch.preflight(instance.id.clone()).await;
    assert_eq!(blocked.error.unwrap().code, LaunchError::InstanceBusy);
    let (before, mut changes) = service.installs.subscribe();
    drop(response);
    release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while changes.borrow_and_update().registry_revision == before.registry_revision {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let current = service
        .enrich(instance, &service.installed().await.unwrap())
        .await;
    assert!(current.launchable, "{}", current.status_detail);
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_setup_result_invalidates_released_admission_without_claiming_success() {
    let (_root, service, work) = accepted_setup_fixture().await;
    let id = work.instance().record().instance.id.clone();
    let before = service.installs.snapshot();
    let task = service
        .instances
        .tasks
        .try_spawn(work, |_| async { Err(InstanceError::SetupUnavailable) })
        .unwrap();
    assert!(matches!(
        service.finish_setup(task).await,
        Err(InstanceError::SetupUnavailable)
    ));
    assert!(service.installs.snapshot().registry_revision > before.registry_revision);
    assert!(has_pending(service.instances.registry().storage(), &id).unwrap());
    assert!(
        service
            .instances
            .directories()
            .exclusions()
            .try_acquire([id.as_str()], [])
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn panicked_setup_keeps_admission_without_publishing_release() {
    let (_root, service, work) = accepted_setup_fixture().await;
    let id = work.instance().record().instance.id.clone();
    let before = service.installs.snapshot();
    let task = service
        .instances
        .tasks
        .try_spawn(work, |_| async {
            panic!("accepted setup interrupted before settlement");
        })
        .unwrap();
    assert!(matches!(
        service.finish_setup(task).await,
        Err(InstanceError::SettlementRequired)
    ));
    assert_eq!(
        service.installs.snapshot().registry_revision,
        before.registry_revision
    );
    assert!(
        service
            .instances
            .directories()
            .exclusions()
            .try_acquire([id.as_str()], [])
            .is_err()
    );
    assert_eq!(service.instances.tasks.status().unsettled.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_setup_metadata_blocks_an_otherwise_launchable_instance() {
    use std::os::unix::fs::PermissionsExt;

    let (root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Readiness").await;
    let versions = service.installed().await.unwrap();
    accounts.create_offline_account("FixturePlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(
        &java,
        format!(
            "#!/bin/sh\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": service.settings.current().unwrap().revision,
                "java_path_override": java.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let ready = service.enrich(instance.clone(), &versions).await;
    assert!(ready.launchable, "{}", ready.status_detail);
    assert_eq!(ready.launch_action.primary_action, "launch");
    service
        .instances
        .registry()
        .storage()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("DROP TABLE instance_setups")?;
            Ok(())
        })
        .unwrap();
    let preflight = service.launch.preflight(instance.id.clone()).await;
    assert!(!preflight.launchable);
    assert_eq!(preflight.error.unwrap().code, LaunchError::InstanceChanged);
    for catalog in std::iter::once(versions.clone()).chain(unready_catalogs(&versions)) {
        let blocked = service.enrich(instance.clone(), &catalog).await;
        assert!(!blocked.launchable);
        assert!(!blocked.launch_action.launchable);
        assert_eq!(blocked.launch_action.state_id, "blocked");
        assert_eq!(blocked.launch_action.primary_action, "blocked");
        assert_eq!(blocked.launch_action.label, "Unavailable");
        assert!(blocked.needs_install.is_empty());
        assert_eq!(
            blocked.status_detail,
            "Instance setup status could not be read. Refresh and try again."
        );
        assert_eq!(
            blocked.launch_action.disabled_reason.as_deref(),
            Some(blocked.status_detail.as_str())
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn grouped_readiness_preserves_row_results_order_and_waiter_scope() {
    use std::os::unix::fs::PermissionsExt;

    let (root, service, accounts) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    accounts.create_offline_account("GroupedPlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(&java, format!(
        "#!/bin/sh\nprobe_dir=${{0%/*}}\nprintf 'probe\\n' >> \"$probe_dir/probes\"\nwhile [ ! -f \"$probe_dir/probe-release\" ]; do sleep 0.01; done\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
        std::env::consts::ARCH,
    )).unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.path().join("probe-release"), b"release").unwrap();
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision": service.settings.current().unwrap().revision,
                "java_path_override": java.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let first = super::super::create::tests::create(&service.instances, "First ready").await;
    let bad = super::super::create::tests::create(&service.instances, "Bad Java").await;
    let bad = service
        .instances
        .update(
            &bad.id,
            super::super::model::InstancePatch {
                java_path: Some(root.path().join("missing-java").to_str().unwrap().into()),
                ..Default::default()
            },
        )
        .unwrap();
    let last = super::super::create::tests::create(&service.instances, "Last ready").await;
    let missing = service
        .instances
        .create(
            CreateInstanceRequest {
                name: "Missing install".into(),
                selection_id: "vanilla|1.20.1".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "vanilla|1.20.1".into(),
                version_id: "1.20.1".into(),
                minecraft_version: "1.20.1".into(),
                loader_key: "vanilla".into(),
            },
            service
                .instances
                .creation_admission_for_tests()
                .await
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let input = vec![first.clone(), missing.clone(), bad.clone(), last.clone()];
    service
        .launch
        .installed_scans
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let rows = service.enrich_all(input.clone()).await.unwrap();
    assert_eq!(
        rows.iter().map(|row| &row.instance.id).collect::<Vec<_>>(),
        input.iter().map(|row| &row.id).collect::<Vec<_>>()
    );
    assert!(rows[0].launchable && rows[3].launchable);
    assert_eq!(rows[1].launch_action.primary_action, "install");
    assert_eq!(rows[1].needs_install, "1.20.1");
    assert_eq!(rows[2].launch_action.primary_action, "blocked");
    assert_eq!(
        rows[2].status_detail,
        "The selected Java executable is missing."
    );
    assert_eq!(
        service
            .launch
            .fresh_install_checks
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(
        service
            .launch
            .installed_scans
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "one request must scan installed metadata once for display and readiness"
    );
    let versions = service.installed().await.unwrap();
    assert!(service.enrich(first.clone(), &versions).await.launchable);
    assert_eq!(
        service
            .launch
            .fresh_install_checks
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );

    // A dropped list finishes its accepted row, not the remaining group.
    std::fs::remove_file(root.path().join("probe-release")).unwrap();
    std::fs::write(root.path().join("probes"), b"").unwrap();
    let service = Arc::new(service);
    let waiter = tokio::spawn({
        let service = service.clone();
        async move { service.enrich_all(vec![first, last]).await.unwrap() }
    });
    let started = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while std::fs::read(root.path().join("probes"))
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    waiter.abort();
    assert!(waiter.await.is_err_and(|error| error.is_cancelled()));
    let pin = service.instances.directories().library().admit().unwrap();
    let artifact = crate::install::queue::library_artifact(&pin.library_id().to_string());
    let blocked = service
        .instances
        .directories()
        .exclusions()
        .try_acquire(std::iter::empty::<String>(), [artifact.clone()])
        .is_err();
    std::fs::write(root.path().join("probe-release"), b"release").unwrap();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !service.instances.tasks.status().is_idle() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(started.is_ok() && blocked && drained.is_ok());
    assert!(!service.instances.tasks.status().closing);
    assert_eq!(
        std::fs::read_to_string(root.path().join("probes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(
        service
            .instances
            .directories()
            .exclusions()
            .try_acquire(std::iter::empty::<String>(), [artifact])
            .is_ok()
    );
}

#[tokio::test]
async fn grouped_readiness_scans_empty_and_busy_lists_without_erasing_rows() {
    use std::sync::atomic::Ordering;

    let (_root, service, _) = fixture();
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let instance = super::super::create::tests::create(&service.instances, "Busy row").await;
    service.launch.installed_scans.store(0, Ordering::Relaxed);
    for expected in 1..=2 {
        assert!(service.enrich_all(Vec::new()).await.unwrap().is_empty());
        assert_eq!(
            service.launch.installed_scans.load(Ordering::Relaxed),
            expected
        );
    }
    let pin = service.instances.directories().library().admit().unwrap();
    let blocker = service
        .instances
        .directories()
        .exclusions()
        .try_acquire(
            std::iter::empty::<String>(),
            [crate::install::queue::library_artifact(
                &pin.library_id().to_string(),
            )],
        )
        .unwrap();
    let rows = service.enrich_all(vec![instance.clone()]).await.unwrap();
    let operation = pin.managed_library().unwrap();
    let publishing =
        axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
    let unavailable = service.enrich_all(Vec::new()).await;
    drop((publishing, operation, blocker, pin));
    service
        .instances
        .tasks
        .shutdown(std::time::Duration::from_secs(3))
        .await
        .unwrap();

    assert_eq!(service.launch.installed_scans.load(Ordering::Relaxed), 4);
    assert_eq!(
        service.launch.fresh_install_checks.load(Ordering::Relaxed),
        0
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instance.id, instance.id);
    assert_eq!(rows[0].version_display.minecraft_label, "1.21.4");
    assert!(!rows[0].launchable);
    assert_eq!(rows[0].launch_action.primary_action, "blocked");
    assert!(rows[0].needs_install.is_empty());
    assert!(unavailable.unwrap().is_empty());
}

#[tokio::test]
#[cfg(unix)]
async fn grouped_readiness_refuses_earlier_inventory_drift_during_a_later_probe() {
    use std::os::unix::fs::PermissionsExt;

    let (root, service, accounts) = fixture();
    let service = Arc::new(service);
    for version in ["1.20.1", "1.21.4"] {
        crate::install::queue::tests::install_ready_fixture(&service.installs, version).await;
    }
    accounts.create_offline_account("GroupedPlayer").unwrap();
    let java = root.path().join("java");
    std::fs::write(&java, format!(
        "#!/bin/sh\nprobe_dir=${{0%/*}}\nprintf 'probe\\n' >> \"$probe_dir/probes\"\nif [ -s \"$probe_dir/probe-first\" ]; then\n  while [ ! -s \"$probe_dir/probe-release\" ]; do sleep 0.01; done\n  major=21\nelse\n  printf 'first' > \"$probe_dir/probe-first\"\n  major=17\nfi\nprintf 'java.version = %s.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' \"$major\" >&2\n",
        std::env::consts::ARCH,
    )).unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.path().join("probe-release"), b"release").unwrap();
    service
        .settings
        .update(
            serde_json::from_value(serde_json::json!({
                "expected_revision":service.settings.current().unwrap().revision,
                "java_path_override":java.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    let first = service
        .instances
        .create(
            CreateInstanceRequest {
                name: "Earlier version".into(),
                selection_id: "vanilla|1.20.1".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "vanilla|1.20.1".into(),
                version_id: "1.20.1".into(),
                minecraft_version: "1.20.1".into(),
                loader_key: "vanilla".into(),
            },
            service
                .instances
                .creation_admission_for_tests()
                .await
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let last = super::super::create::tests::create(&service.instances, "Later version").await;
    let input = vec![first, last];
    let healthy = service.enrich_all(input.clone()).await.unwrap();
    assert!(healthy.iter().all(|row| row.launchable), "{healthy:?}");
    let asset = root
        .path()
        .join("assets/log_configs/guardian-version-bundle.xml");
    let original = std::fs::read(&asset).unwrap();
    std::fs::write(root.path().join("probes"), b"").unwrap();
    std::fs::write(root.path().join("probe-first"), b"").unwrap();
    std::fs::write(root.path().join("probe-release"), b"").unwrap();
    let waiter = tokio::spawn({
        let service = service.clone();
        let input = input.clone();
        async move { service.enrich_all(input).await.unwrap() }
    });
    let paused = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while std::fs::read_to_string(root.path().join("probes"))?
            .lines()
            .count()
            < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    let pending = !waiter.is_finished();
    let changed = std::fs::write(&asset, b"external change");
    let released = std::fs::write(root.path().join("probe-release"), b"release");
    let rows = waiter.await;
    let stopped = service
        .instances
        .tasks
        .shutdown(std::time::Duration::from_secs(5))
        .await;
    assert!(
        matches!(paused, Ok(Ok(()))) && pending,
        "second probe did not remain pending: {paused:?}"
    );
    changed.unwrap();
    released.unwrap();
    stopped.unwrap();
    let rows = rows.unwrap();
    assert_eq!(std::fs::read(&asset).unwrap(), b"external change");
    assert_ne!(original, b"external change");
    assert_eq!(rows.len(), input.len());
    for (row, instance) in rows.iter().zip(&input) {
        assert_eq!(row.instance.id, instance.id);
        assert!(!row.launchable, "{} retained stale Launch", instance.name);
        assert!(!row.launch_action.launchable);
    }
}

#[test]
#[cfg(unix)]
fn grouped_readiness_blocks_early_rows_when_the_library_scan_changes() {
    check_grouped_readiness_invalidated(false);
}

#[test]
#[cfg(unix)]
fn grouped_readiness_blocks_cached_rows_when_publication_starts() {
    check_grouped_readiness_invalidated(true);
}

#[cfg(unix)]
fn check_grouped_readiness_invalidated(publication_starts: bool) {
    use futures_util::FutureExt;
    use std::os::unix::fs::PermissionsExt;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (root, service, accounts) = runtime.block_on(async { fixture() });
    let service = Arc::new(service);
    let mut waiter = None;
    let mut publication = None;
    let journey = runtime.block_on(std::panic::AssertUnwindSafe(async {
        crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
        accounts.create_offline_account("GroupedPlayer").unwrap();
        let java = root.path().join("java");
        std::fs::write(&java, format!(
            "#!/bin/sh\nprobe_dir=${{0%/*}}\nprintf 'probe\\n' >> \"$probe_dir/probes\"\nif [ -s \"$probe_dir/probe-first\" ]; then\n  while [ ! -s \"$probe_dir/probe-release\" ]; do sleep 0.01; done\nelse\n  printf 'first' > \"$probe_dir/probe-first\"\nfi\nprintf 'java.version = 21.0.3\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        )).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.path().join("probe-first"), b"").unwrap();
        std::fs::write(root.path().join("probe-release"), b"release").unwrap();
        service.settings.update(serde_json::from_value(serde_json::json!({
            "expected_revision":service.settings.current().unwrap().revision,
            "java_path_override":java.to_str().unwrap(),
        })).unwrap()).unwrap();
        let first = super::super::create::tests::create(&service.instances, "First healthy").await;
        let last = super::super::create::tests::create(&service.instances, "Last healthy").await;
        let input = vec![first.clone(), last];
        let lane = root.path().join(".axial-publication");
        let saved_lane = root.path().join("previous-publication");
        if publication_starts {
            std::fs::rename(&lane, &saved_lane).unwrap();
        }
        let healthy = service.enrich_all(input.clone()).await.unwrap();
        let lane_absent = std::fs::symlink_metadata(&lane)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let protected: Vec<_> = ["json", "jar"].into_iter().map(|extension| {
            let path = root.path().join(format!("versions/1.21.4/1.21.4.{extension}"));
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        }).collect();
        let versions_root = root.path().join("versions");
        let revision_before = std::fs::metadata(&versions_root).unwrap().modified().unwrap();
        std::fs::write(root.path().join("probes"), b"").unwrap();
        std::fs::write(root.path().join("probe-first"), b"").unwrap();
        std::fs::write(root.path().join("probe-release"), b"").unwrap();
        waiter = Some(tokio::spawn({
            let service = service.clone();
            let input = input.clone();
            async move { service.enrich_all(input).await.unwrap() }
        }));
        // The first row has completed before the second actual Java probe pauses.
        let paused = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while std::fs::read_to_string(root.path().join("probes")).unwrap().lines().count() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await;
        let still_pending = !waiter.as_ref().unwrap().is_finished();
        let probe_count = std::fs::read_to_string(root.path().join("probes")).unwrap().lines().count();
        let external = versions_root.join("external-degraded-entry");
        let absent_before = std::fs::symlink_metadata(&external)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let metadata = external.join("external-degraded-entry.json");
        let malformed = b"{not valid external version metadata\n";
        if publication_starts {
            std::fs::rename(&saved_lane, &lane).unwrap();
            let pin = service.instances.directories().library().admit().unwrap();
            let operation = pin.managed_library().unwrap();
            let guard = axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).unwrap();
            publication = Some((guard, operation, pin));
        } else {
            std::fs::create_dir(&external).unwrap();
            std::fs::write(&metadata, malformed).unwrap();
        }
        let revision_after = std::fs::metadata(&versions_root).unwrap().modified().unwrap();
        std::fs::write(root.path().join("probe-release"), b"release").unwrap();
        let rows = waiter.take().unwrap().await.unwrap();
        let degraded = serde_json::to_value(service.launch.preflight(first.id.clone()).await).unwrap();
        let degraded_rows = service.enrich_all(input.clone()).await.unwrap();
        let preserved = if publication_starts {
            drop(publication.take());
            None
        } else {
            let bytes = std::fs::read(&metadata).unwrap();
            std::fs::remove_file(&metadata).unwrap();
            std::fs::remove_dir(&external).unwrap();
            Some(bytes)
        };
        let restored = service.enrich_all(input.clone()).await.unwrap();
        move || {
            assert!(paused.is_ok() && still_pending, "second probe did not remain pending");
            assert_eq!(probe_count, 2);
            assert!(absent_before);
            if publication_starts {
                assert!(lane_absent, "cached rows must capture the absent publication lane");
                assert_eq!(revision_after, revision_before, "version scan changed during publication control");
                assert!(preserved.is_none());
            } else {
                assert_ne!(revision_after, revision_before, "directory revision did not advance");
                assert_eq!(preserved.unwrap(), malformed);
            }
            assert!(std::fs::symlink_metadata(external)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound));
            for (path, bytes) in protected {
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            for ready in [&healthy, &restored] {
                assert_eq!(ready.len(), input.len());
                for (row, instance) in ready.iter().zip(&input) {
                    assert_eq!(row.instance.id, instance.id);
                    assert!(row.launchable, "{}", row.status_detail);
                    assert!(row.launch_action.launchable);
                    assert_eq!(row.launch_action.primary_action, "launch");
                    assert!(row.needs_install.is_empty());
                }
            }
            assert_eq!(degraded_rows.len(), input.len());
            for (row, instance) in degraded_rows.iter().zip(&input) {
                assert_eq!(row.instance.id, instance.id);
                assert_eq!(row.version_display.minecraft_label, "1.21.4");
                assert!(!row.launchable);
                assert_eq!(row.launch_action.primary_action, "blocked");
                assert!(row.needs_install.is_empty());
            }
            assert_eq!(degraded["status"], "ready", "{degraded}");
            assert_eq!(degraded["launchable"], false);
            let (reason, message) = if publication_starts {
                ("incomplete_install", "Installation is changing. Wait for it to finish before launching.")
            } else {
                ("installed_versions_degraded", "Could not verify installed versions. Check the library folder and try again.")
            };
            assert_eq!(degraded["readiness"], serde_json::json!({
                "launchable":false,"reasons":[{
                    "id":reason,"severity":"blocking","message":message
                }]
            }));
            assert_eq!(rows.len(), input.len());
            for (row, instance) in rows.iter().zip(&input) {
                assert_eq!(row.instance.id, instance.id);
                assert!(!row.launchable, "{} retained stale Launch", instance.name);
                assert!(!row.launch_action.launchable);
                assert_eq!(row.launch_action.primary_action, "blocked");
                assert!(row.needs_install.is_empty());
            }
        }
    }).catch_unwind());
    let released = std::fs::write(root.path().join("probe-release"), b"release");
    let remaining = runtime.block_on(async {
        match waiter.take() {
            Some(waiter) => Some(waiter.await),
            None => None,
        }
    });
    drop(publication.take());
    let shutdown = runtime.block_on(
        std::panic::AssertUnwindSafe(
            service
                .instances
                .tasks
                .shutdown(std::time::Duration::from_secs(5)),
        )
        .catch_unwind(),
    );
    let settled = matches!(&shutdown, Ok(Ok(()))) && service.instances.tasks.status().is_idle();
    if !settled {
        std::mem::forget((service, accounts, runtime));
    }
    let verification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let verify = match journey {
            Ok(verify) => verify,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        assert!(released.is_ok());
        if let Some(result) = remaining {
            assert!(result.is_ok(), "enrichment waiter did not join");
        }
        match shutdown {
            Ok(result) => assert!(result.is_ok(), "{result:?}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
        assert!(settled);
        verify();
    }));
    if let Err(panic) = verification {
        use std::io::Write;
        let retained = root.keep();
        let _ = writeln!(
            std::io::stderr(),
            "Retained grouped readiness fixture: {}",
            retained.display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn create_view_download_indicators_use_settled_install_and_scanner_status() {
    let (root, mut service, _) = fixture();
    service.loader_catalog_fixture = Some(stale_loader_catalog());
    let service = Arc::new(service);
    let initial = service.create_view(None).await.unwrap();
    assert!(
        initial
            .versions
            .iter()
            .all(|row| row.download_state == "none")
    );
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    let installed = service.create_view(None).await.unwrap();
    assert_eq!(
        installed
            .versions
            .iter()
            .find(|row| row.minecraft_version_id == "1.21.4")
            .unwrap()
            .download_state,
        "full"
    );
    let loader = service
        .create_view(Some(LoaderComponentId::Fabric.as_str()))
        .await
        .unwrap();
    assert_eq!(loader.versions[0].download_state, "base");
    assert!(
        !loader.versions[0].create_enabled,
        "the base does not authorize a stale loader choice"
    );
    std::fs::write(root.path().join("versions/1.21.4/1.21.4.jar"), b"changed").unwrap();
    let drifted = service.create_view(None).await.unwrap();
    assert_eq!(
        drifted
            .versions
            .iter()
            .find(|row| row.minecraft_version_id == "1.21.4")
            .unwrap()
            .download_state,
        "full",
        "displayed installation is not a full-payload integrity attestation"
    );
    std::fs::remove_file(root.path().join("versions/1.21.4/1.21.4.jar")).unwrap();
    let incomplete = service.create_view(None).await.unwrap();
    assert_eq!(
        incomplete
            .versions
            .iter()
            .find(|row| row.minecraft_version_id == "1.21.4")
            .unwrap()
            .download_state,
        "none",
        "a settled metadata row alone must not override an incomplete scan"
    );
}

#[tokio::test]
async fn offline_loader_creation_reuses_only_the_exact_preferred_verified_build() {
    let (root, mut service, _) = fixture();
    let catalog = stale_loader_catalog();
    let installed = catalog.builds[0].clone();
    service.loader_catalog_fixture = Some(catalog);
    let mut service = Arc::new(service);
    let selection = "loader_auto|net.fabricmc.fabric-loader|1.21.4";
    assert!(matches!(
        service.resolve(selection).await,
        Err(InstanceError::VersionUnavailable)
    ));
    let absent = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!absent.auto.enabled);
    assert!(absent.auto.disabled_reason.is_some());
    assert!(!absent.builds[0].enabled);
    crate::install::queue::tests::install_ready_fixture(&service.installs, "1.21.4").await;
    crate::install::queue::tests::install_ready_fixture(&service.installs, &installed.version_id)
        .await;
    let scanned = service.installed().await.unwrap();
    assert!(scanned.iter().any(|entry| {
        entry.id == installed.version_id
            && entry.launchable
            && entry.loader.as_ref().is_some_and(|loader| {
                loader.component_id == installed.component_id
                    && loader.build_id == installed.build_id
            })
    }));
    let loader_rows = service
        .create_view(Some(LoaderComponentId::Fabric.as_str()))
        .await
        .unwrap();
    assert_eq!(loader_rows.versions[0].download_state, "full");
    assert!(loader_rows.versions[0].create_enabled);
    let available = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(available.auto.enabled);
    assert!(available.auto.disabled_reason.is_none());
    assert!(available.builds[0].enabled && available.builds[0].installed);
    let before = service.installs.snapshot();
    let (target, request, _) = service.resolve(selection).await.unwrap();
    assert_eq!(target.version_id(), installed.version_id);
    assert_eq!(
        request,
        InstallQueueRequest::Loader {
            component_id: installed.component_id,
            build_id: installed.build_id.clone()
        }
    );
    let exact = format!(
        "loader_build|{}|{}",
        installed.component_id.as_str(),
        installed.build_id
    );
    assert_eq!(
        service.resolve(&exact).await.unwrap().0.version_id(),
        installed.version_id
    );
    let mut newer = installed.clone();
    newer.loader_version = "0.16.15".into();
    newer.build_id = loaders::build_id_for(newer.component_id, "1.21.4", "0.16.15");
    newer.version_id =
        loaders::installed_version_id_for(newer.component_id, "1.21.4", "0.16.15").unwrap();
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds = vec![newer, installed.clone()];
    assert!(
        matches!(
            service.resolve(selection).await,
            Err(InstanceError::VersionUnavailable)
        ),
        "automatic selection must not substitute an older installed build for the preferred build"
    );
    let choices = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!choices.auto.enabled);
    assert!(choices.auto.disabled_reason.is_some());
    assert!(choices.builds[0].recommended && !choices.builds[0].enabled);
    assert!(choices.builds[1].installed && choices.builds[1].enabled);
    assert_eq!(
        service.resolve(&exact).await.unwrap().0.version_id(),
        installed.version_id,
        "the older explicit installed choice remains usable"
    );
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .state
        .availability
        .fresh = true;
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .state
        .availability
        .stale = false;
    let fresh = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(fresh.auto.enabled && fresh.builds[0].enabled);
    Arc::get_mut(&mut service)
        .unwrap()
        .loader_catalog_fixture
        .as_mut()
        .unwrap()
        .builds
        .clear();
    let empty = service
        .loader_builds(LoaderComponentId::Fabric.as_str(), "1.21.4")
        .await
        .unwrap();
    assert!(!empty.auto.enabled && empty.auto.disabled_reason.is_some());
    Arc::get_mut(&mut service).unwrap().loader_catalog_fixture = Some(stale_loader_catalog());
    let created = service
        .create(CreateInstanceRequest {
            name: "Offline Fabric".into(),
            selection_id: selection.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.instance.instance.version_id, installed.version_id);
    assert!(created.install_queue.is_none());
    assert_eq!(service.installs.snapshot(), before);
    std::fs::write(
        root.path()
            .join(format!("versions/{0}/{0}.jar", installed.version_id)),
        b"changed",
    )
    .unwrap();
    assert!(matches!(
        service.resolve(selection).await,
        Err(InstanceError::VersionUnavailable)
    ));
}

#[tokio::test]
async fn first_instance_still_queues_uninstalled_runtime() {
    let (_root, service, _) = fixture();
    let pin = service.instances.directories().library().admit().unwrap();
    let blocker = service
        .instances
        .directories()
        .exclusions()
        .try_acquire_read_artifacts(
            std::iter::empty::<String>(),
            [crate::install::queue::library_artifact(
                &pin.library_id().to_string(),
            )],
        )
        .unwrap();
    let service = Arc::new(service);
    let created = service
        .create(super::super::create::tests::request("First installation"))
        .await
        .unwrap();
    let queue = created.install_queue.unwrap();
    assert!(queue.active.is_none());
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].install_item.version_id, "1.21.4");
    assert_eq!(created.instance.launch_action.primary_action, "install");
    service.installs.shutdown_queued().unwrap();
    drop(blocker);
}
