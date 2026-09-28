use super::*;

fn fixture_directory() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

fn suite() -> BenchmarkSuiteManifest {
    BenchmarkSuiteManifest {
        schema: "axial.launch.benchmark.suite".into(),
        schema_version: 2,
        suite_id: format!("legacy-suite-{}", "1".repeat(64)),
        instance_id: "1c53a187-80d2-4396-9dc8-1d11dc3f03d0".into(),
        mode: "release_validation".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:05:00Z".into(),
        historical: true,
        runs: vec![BenchmarkSuiteManifestRun {
            run_index: 5,
            profile: "retained+custom".into(),
            run_type: "warm".into(),
            target_id: "older+target".into(),
            benchmark_id: "benchmark-1234567890abcdef".into(),
            session_id: Some(format!("legacy-{}", "a".repeat(64))),
            launched_at: Some("2026-01-01T00:01:00Z".into()),
            state: "completed".into(),
            launch_intent: None,
        }],
    }
}

fn driver() -> BenchmarkSuiteDriverStatus {
    BenchmarkSuiteDriverStatus {
        id: format!("legacy-driver-{}", "2".repeat(64)),
        suite_id: suite().suite_id,
        mode: "release_validation".into(),
        state: "interrupted".into(),
        interval_ms: 5_000,
        run_count: 8,
        launched_run_count: 1,
        pending_run_index: Some(1),
        active_session_id: None,
        last_run_index: Some(5),
        last_session_id: Some(format!("legacy-{}", "b".repeat(64))),
        error: Some("Driver interrupted by application restart".into()),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:03:00Z".into(),
        historical: true,
    }
}

fn open_storage(path: &std::path::Path) -> Arc<MetadataStore> {
    let storage = Arc::new(MetadataStore::open(path).unwrap());
    storage.migrate(&[MIGRATION, MIGRATION_V2]).unwrap();
    storage
}

#[test]
fn imported_history_reopens_with_exact_plan_and_immutable_retry() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let prepared = PreparedBenchmarkImport::prepare(vec![suite()], vec![driver()]).unwrap();
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    drop(storage);
    let storage = open_storage(&path);
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    storage
        .read(|db| {
            assert_eq!(stored_suite(db, &suite().suite_id)?, Some(suite()));
            assert_eq!(stored_driver(db, &driver().id)?, Some(driver()));
            let count: i64 = db.query_row(
                "SELECT count(*) FROM benchmark_drivers WHERE request IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0);
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    let view = driver_payload(driver());
    assert_eq!(
        view["view_model"]["state_label"],
        "Historical interrupted (read-only)"
    );
    assert_eq!(view["view_model"]["can_stop"], false);
    assert_eq!(view["view_model"]["can_resume"], false);
    assert_eq!(view["view_model"]["can_check_family_c_qualification"], true);
    assert_eq!(view["suite"]["pending_run_index"], 1);
    assert!(
        serde_json::to_value(suite()).unwrap()["runs"][0]
            .get("launch_intent")
            .is_none()
    );
}

#[test]
fn imported_history_conflict_rolls_back_whole_batch_and_rejects_changed_request() {
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    let prepared = PreparedBenchmarkImport::prepare(vec![suite()], vec![driver()]).unwrap();
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    let mut another = suite();
    another.suite_id = format!("legacy-suite-{}", "3".repeat(64));
    another.runs[0].session_id = Some(format!("legacy-{}", "c".repeat(64)));
    let mut conflict = driver();
    conflict.error = Some("Different retained failure".into());
    let changed =
        PreparedBenchmarkImport::prepare(vec![suite(), another.clone()], vec![conflict]).unwrap();
    assert!(matches!(
        storage.transaction(|tx| changed.insert_in(tx)),
        Err(BenchmarkError::ConflictingHistory)
    ));
    storage
        .read(|db| {
            assert!(stored_suite(db, &another.suite_id)?.is_none());
            assert_eq!(stored_driver(db, &driver().id)?, Some(driver()));
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    storage
        .transaction(|tx| {
            tx.execute(
                "UPDATE benchmark_drivers SET request=?1 WHERE driver_id=?2",
                params![b"{}".as_slice(), driver().id],
            )?;
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    assert!(matches!(
        storage.transaction(|tx| prepared.verify_in(tx)),
        Err(BenchmarkError::Unavailable)
    ));
    storage
        .transaction(|tx| {
            tx.execute(
                "DELETE FROM benchmark_drivers WHERE driver_id=?1",
                [driver().id],
            )?;
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    assert!(matches!(
        storage.transaction(|tx| prepared.verify_in(tx)),
        Err(BenchmarkError::ConflictingHistory)
    ));
}

#[test]
fn historical_admission_rejects_runnable_or_ambiguous_records() {
    let mut malformed = suite();
    malformed.runs[0].launch_intent = Some(uuid::Uuid::new_v4().to_string());
    assert!(PreparedBenchmarkImport::prepare(vec![malformed], vec![]).is_err());
    let mut missing_marker = suite();
    missing_marker.historical = false;
    assert!(validate_suite(&missing_marker).is_err());
    let mut missing_driver_marker = driver();
    missing_driver_marker.historical = false;
    assert!(validate_driver(&missing_driver_marker).is_err());
    let mut duplicate_session = suite();
    duplicate_session.suite_id = format!("legacy-suite-{}", "3".repeat(64));
    assert!(PreparedBenchmarkImport::prepare(vec![suite(), duplicate_session], vec![]).is_err());
    let mut queued = driver();
    queued.error = Some("driver automatic resume queued after restart".into());
    assert!(PreparedBenchmarkImport::prepare(vec![suite()], vec![queued]).is_err());
    for marker in [
        "driver automatic resume started after restart",
        "driver ignored after restart resume limit",
    ] {
        let mut interrupted = driver();
        interrupted.error = Some(marker.into());
        PreparedBenchmarkImport::prepare(vec![suite()], vec![interrupted.clone()]).unwrap();
        let view = driver_payload(interrupted.clone());
        assert_eq!(view["driver"]["historical"], true);
        assert_eq!(view["driver"]["error"], marker);
        assert_eq!(view["view_model"]["can_stop"], false);
        assert_eq!(view["view_model"]["can_resume"], false);
        for state in ["complete", "failed", "stopped", "running"] {
            let mut invalid = interrupted.clone();
            invalid.state = state.into();
            invalid.pending_run_index = None;
            assert!(PreparedBenchmarkImport::prepare(vec![suite()], vec![invalid]).is_err());
        }
        interrupted.active_session_id = interrupted.last_session_id.clone();
        assert!(PreparedBenchmarkImport::prepare(vec![suite()], vec![interrupted]).is_err());
    }
    let mut live = driver();
    live.historical = false;
    live.id = "benchmark-suite-driver-existing".into();
    live.suite_id = "suite-existing".into();
    let wire = serde_json::to_value(&live).unwrap();
    assert!(wire.get("historical").is_none());
    assert!(
        !serde_json::from_value::<BenchmarkSuiteDriverStatus>(wire)
            .unwrap()
            .historical
    );
    assert_eq!(driver_payload(live)["view_model"]["can_resume"], true);
}

#[test]
fn historical_pending_plans_preserve_absent_launches_and_reject_ambiguous_runs() {
    let mut pending = suite();
    pending.runs = (0..2)
        .map(|index| {
            let mut run = suite().runs.remove(0);
            run.run_index = index;
            run.benchmark_id = format!("benchmark-{index:016x}");
            run.session_id = None;
            run.launched_at = None;
            run.state = "pending".into();
            run
        })
        .collect();
    let mut stopped = driver();
    stopped.state = "stopped".into();
    stopped.last_run_index = None;
    stopped.last_session_id = None;
    stopped.launched_run_count = 0;
    stopped.pending_run_index = Some(0);
    let mut mixed = suite();
    mixed.suite_id = format!("legacy-suite-{}", "3".repeat(64));
    mixed.runs.extend(pending.runs.clone());
    let prepared = PreparedBenchmarkImport::prepare(
        vec![pending.clone(), mixed.clone()],
        vec![stopped.clone()],
    )
    .unwrap();
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    storage
        .read(|db| {
            assert_eq!(stored_suite(db, &pending.suite_id)?, Some(pending.clone()));
            assert_eq!(stored_suite(db, &mixed.suite_id)?, Some(mixed));
            assert_eq!(stored_driver(db, &stopped.id)?, Some(stopped.clone()));
            let request: Option<Vec<u8>> = db.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [&stopped.id],
                |row| row.get(0),
            )?;
            assert!(request.is_none());
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    for case in [
        "session_only",
        "time_only",
        "pending_with_launch",
        "running",
        "launching",
        "terminal_without_launch",
        "launch_intent",
    ] {
        let mut invalid = pending.clone();
        let run = &mut invalid.runs[0];
        match case {
            "session_only" => run.session_id = suite().runs.remove(0).session_id,
            "time_only" => run.launched_at = suite().runs.remove(0).launched_at,
            "pending_with_launch" => {
                run.session_id = suite().runs.remove(0).session_id;
                run.launched_at = suite().runs.remove(0).launched_at;
            }
            "running" | "launching" => run.state = case.into(),
            "terminal_without_launch" => run.state = "completed".into(),
            "launch_intent" => run.launch_intent = Some(uuid::Uuid::new_v4().to_string()),
            _ => unreachable!(),
        }
        assert!(
            PreparedBenchmarkImport::prepare(vec![invalid], vec![]).is_err(),
            "{case}"
        );
    }
    stopped.pending_run_index = Some(7);
    PreparedBenchmarkImport::prepare(vec![pending.clone()], vec![stopped.clone()]).unwrap();
    stopped.last_run_index = Some(8);
    assert!(PreparedBenchmarkImport::prepare(vec![pending], vec![stopped]).is_err());
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
            crate::instances::create::DUPLICATE_WITNESS_MIGRATION,
            crate::content::install::MIGRATION,
            crate::install::queue::MIGRATION,
            crate::install::queue::MIGRATION_V2,
            crate::performance::rules::MIGRATION,
            crate::performance::mutation::MIGRATION,
            crate::skins::store::MIGRATION,
            crate::launch::coordinator::INTENT_MIGRATION,
            crate::launch::coordinator::INTENT_TERMINAL_MIGRATION,
            crate::launch::coordinator::INTENT_SETTLEMENT_MIGRATION,
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

async fn continuation_service(
    root: &std::path::Path,
    storage: Arc<MetadataStore>,
) -> (BenchmarkService, InstanceId) {
    use crate::instances::create::{CreateInstanceRequest, CreateTarget, InstanceService};
    let (service, directories) = service_with_directories(root, storage);
    let instance = InstanceService::new(directories, service.tasks.clone())
        .create(
            CreateInstanceRequest {
                name: "Imported benchmark continuation".into(),
                selection_id: "1.21.1".into(),
                ..Default::default()
            },
            CreateTarget {
                selection_id: "1.21.1".into(),
                version_id: "1.21.1".into(),
                minecraft_version: "1.21.1".into(),
                loader_key: "vanilla".into(),
            },
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    (service, instance.id)
}

fn continuation_history(
    instance: &InstanceId,
    mixed: bool,
) -> (
    BenchmarkSuiteManifest,
    BenchmarkSuiteDriverStatus,
    Option<crate::launch::reports::LaunchProofRecord>,
) {
    use crate::launch::{
        outcome::{SessionExitReason, SessionOutcome, SessionOutcomeKind},
        reports::{LaunchProofRecord, SessionReportInput},
    };
    let mut source = suite();
    source.instance_id = instance.to_string();
    source.mode = "development".into();
    source.runs = benchmark_suite_plan(&source.mode)
        .unwrap()
        .into_iter()
        .enumerate()
        .map(|(index, run)| BenchmarkSuiteManifestRun {
            run_index: index,
            profile: run.profile.into(),
            run_type: run.run_type.into(),
            target_id: run.target_id.unwrap_or("").into(),
            benchmark_id: benchmark_suite_run_id(&source.mode, index, run),
            session_id: None,
            launched_at: None,
            state: "pending".into(),
            launch_intent: None,
        })
        .collect();
    let mut previous = driver();
    previous.mode = source.mode.clone();
    previous.state = "stopped".into();
    previous.run_count = source.runs.len();
    previous.launched_run_count = usize::from(mixed);
    previous.pending_run_index = Some(usize::from(mixed));
    previous.last_run_index = mixed.then_some(0);
    previous.last_session_id = None;
    previous.error = None;
    let report = mixed.then(|| {
        let run = &mut source.runs[0];
        run.session_id = Some(format!("legacy-{}", "a".repeat(64)));
        run.launched_at = Some("2026-01-01T00:01:00.000Z".into());
        run.state = "exited".into();
        previous.last_session_id = run.session_id.clone();
        let mut outcome = SessionOutcome {
            kind: SessionOutcomeKind::Clean,
            reason: SessionExitReason::CleanExit,
            failure_class: None,
            summary: String::new(),
        };
        outcome.summary = outcome.summary().into();
        let mut report = LaunchProofRecord::from_session(SessionReportInput {
            session_id: run.session_id.clone().unwrap(),
            instance_id: source.instance_id.clone(),
            version_id: "1.21.1".into(),
            launched_at: run.launched_at.clone().unwrap(),
            ended_at: "2026-01-02T00:00:00.000Z".into(),
            outcome,
            entries: Vec::new(),
            exit_code: Some(0),
            boot_duration_ms: None,
            logs_dropped: 0,
        });
        report.scenario =
            benchmark_scenario(&run.profile, &run.run_type, &source.mode, &run.benchmark_id);
        report.stages.push(serde_json::from_value(serde_json::json!({
            "stage":"imported_history", "label":"Imported terminal evidence", "started_at_ms":0,
            "ended_at_ms":null, "duration_ms":null, "result":null, "warnings":[], "fallback_reason":null,
            "evidence":[
                {"id":"original_session", "system":"history", "summary":"Original session", "details":["session-a"]},
                {"id":"original_outcome", "system":"history", "summary":"Original terminal report outcome", "details":["exited"]},
                {"id":"original_reason", "system":"history", "summary":"Original exit reason", "details":["clean_exit"]}
            ]
        })).unwrap());
        report
    });
    (source, previous, report)
}

fn insert_continuation_history(
    storage: &MetadataStore,
    source: &BenchmarkSuiteManifest,
    previous: &BenchmarkSuiteDriverStatus,
    report: Option<&crate::launch::reports::LaunchProofRecord>,
) -> PreparedBenchmarkImport {
    let prepared =
        PreparedBenchmarkImport::prepare(vec![source.clone()], vec![previous.clone()]).unwrap();
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    let reports = crate::launch::reports::PreparedReportImport::prepare(
        report.into_iter().cloned().collect(),
    )
    .unwrap();
    storage.transaction(|tx| reports.insert_in(tx)).unwrap();
    prepared
}

async fn explicit_imported_driver_resume(mixed: bool, terminal_state: &str) {
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    let (service, instance) = continuation_service(root.path(), storage.clone()).await;
    let (mut source, previous, mut report) = continuation_history(&instance, mixed);
    if let Some(report) = &mut report {
        source.runs[0].state = terminal_state.into();
        report.stages[0].evidence[1].details[0] = terminal_state.into();
    }
    let prepared = insert_continuation_history(&storage, &source, &previous, report.as_ref());
    assert_eq!(service.resume_interrupted_drivers().unwrap(), 0);
    assert!(service.can_resume_driver(&previous.id).unwrap());
    assert!(service.resumed_driver(&previous.id).unwrap().is_none());

    let successor = service
        .resume_driver(&previous.id)
        .expect("explicit Resume must admit a separate operational successor");
    assert!(!successor.historical);
    assert_ne!(successor.id, previous.id);
    assert_ne!(successor.suite_id, source.suite_id);
    assert_eq!(successor.run_count, source.runs.len());
    assert_eq!(successor.launched_run_count, usize::from(mixed));
    assert_eq!(successor.interval_ms, previous.interval_ms);
    let operational = service.suite(&successor.suite_id).unwrap().unwrap();
    assert!(!operational.historical);
    assert_eq!(operational.instance_id, source.instance_id);
    assert_eq!(operational.mode, source.mode);
    assert_eq!(operational.runs.len(), source.runs.len());
    for (run, retained) in operational.runs.iter().zip(&source.runs) {
        assert_eq!(run.run_index, retained.run_index);
        assert_eq!(run.benchmark_id, retained.benchmark_id);
        if retained.state == "pending" {
            assert_eq!(run.state, "pending");
            assert!(run.session_id.is_none());
            assert!(run.launched_at.is_none());
            assert!(run.launch_intent.is_some());
        } else {
            assert_eq!(run, retained);
            assert!(run.launch_intent.is_none());
        }
    }
    let retry = service.resume_driver(&previous.id).unwrap();
    assert_eq!(retry.id, successor.id);
    assert_eq!(retry.suite_id, successor.suite_id);
    assert_eq!(service.resumed_driver(&previous.id).unwrap(), Some(retry));
    assert!(!service.can_resume_driver(&previous.id).unwrap());
    storage
        .read(|db| {
            let bytes: Vec<u8> = db.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [&successor.id],
                |row| row.get(0),
            )?;
            let captured: BenchmarkLaunchRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(captured.instance_id, Some(instance));
            assert_eq!(
                captured.suite_id.as_deref(),
                Some(successor.suite_id.as_str())
            );
            assert_eq!(captured.suite_mode.as_deref(), Some(source.mode.as_str()));
            assert!(captured.username.is_none());
            assert!(captured.max_memory_mb.is_none());
            assert!(captured.min_memory_mb.is_none());
            assert!(captured.client_started_at_ms.is_none());
            let source_request: Option<Vec<u8>> = db.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [&previous.id],
                |row| row.get(0),
            )?;
            assert!(source_request.is_none());
            for table in ["benchmark_suites", "benchmark_drivers"] {
                let count: i64 =
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 2);
            }
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    // Admission is the boundary under test; no installed runtime or successful executor is invented.
    service.stop_driver(&successor.id).unwrap();
    service
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    if let Some(report) = report {
        assert_eq!(
            service.reports.get(&report.session_id).unwrap(),
            Some(report)
        );
    }
    assert!(service.sessions.sessions().is_empty());
    storage
        .read(|db| {
            let count: i64 =
                db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get(0))?;
            assert_eq!(count, 0);
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
}

#[tokio::test]
async fn explicit_imported_driver_resume_accepts_all_pending_current_plan() {
    explicit_imported_driver_resume(false, "exited").await;
}

#[tokio::test]
async fn explicit_imported_driver_resume_accepts_mixed_terminal_and_pending_current_plan() {
    explicit_imported_driver_resume(true, "exited").await;
}

#[tokio::test]
async fn explicit_imported_driver_resume_accepts_original_completed_outcome() {
    explicit_imported_driver_resume(true, "completed").await;
}

#[tokio::test]
async fn explicit_imported_driver_resume_refuses_unsupported_or_contradictory_evidence() {
    for case in [
        "missing_report",
        "wrong_instance",
        "wrong_descriptor",
        "wrong_outcome",
        "wrong_launch_time",
        "unsupported_plan",
    ] {
        let root = fixture_directory();
        let storage = open_storage(&root.path().join("metadata.sqlite"));
        let (service, instance) = continuation_service(root.path(), storage.clone()).await;
        let (mut source, previous, mut report) = continuation_history(&instance, true);
        match case {
            "missing_report" => report = None,
            "wrong_instance" => {
                report.as_mut().unwrap().instance_id = InstanceId::new().to_string()
            }
            "wrong_descriptor" => {
                report.as_mut().unwrap().scenario.benchmark_profile = Some("managed_default".into())
            }
            "wrong_outcome" => source.runs[0].state = "failed".into(),
            "wrong_launch_time" => {
                report.as_mut().unwrap().launched_at = "2026-01-01T00:06:00.000Z".into()
            }
            "unsupported_plan" => source.runs[1].profile = "retained+custom".into(),
            _ => unreachable!(),
        }
        let prepared = insert_continuation_history(&storage, &source, &previous, report.as_ref());
        assert!(!service.can_resume_driver(&previous.id).unwrap(), "{case}");
        assert!(service.resume_driver(&previous.id).is_err(), "{case}");
        storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
        assert_eq!(service.drivers().unwrap(), vec![previous]);
        assert!(service.tasks.status().is_idle());
        assert!(service.sessions.sessions().is_empty());
        storage
            .read(|db| {
                let count: i64 =
                    db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get(0))?;
                assert_eq!(count, 0, "{case}");
                let count: i64 =
                    db.query_row("SELECT count(*) FROM benchmark_suites", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 1, "{case}");
                Ok::<_, BenchmarkError>(())
            })
            .unwrap();
    }
}

#[test]
fn continuation_migration_preserves_existing_history_without_authority() {
    let root = fixture_directory();
    let storage = MetadataStore::open(root.path().join("metadata.sqlite")).unwrap();
    storage.migrate(&[MIGRATION]).unwrap();
    storage
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO benchmark_suites(suite_id,payload) VALUES(?1,?2)",
                params![suite().suite_id, serde_json::to_vec(&suite()).unwrap()],
            )?;
            tx.execute(
                "INSERT INTO benchmark_drivers(driver_id,payload,request) VALUES(?1,?2,NULL)",
                params![driver().id, serde_json::to_vec(&driver()).unwrap()],
            )?;
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    storage.migrate(&[MIGRATION, MIGRATION_V2]).unwrap();
    storage.migrate(&[MIGRATION, MIGRATION_V2]).unwrap();
    let prepared = PreparedBenchmarkImport::prepare(vec![suite()], vec![driver()]).unwrap();
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    storage
        .read(|db| {
            for (table, column) in [
                ("benchmark_suites", "source_suite_id"),
                ("benchmark_drivers", "source_driver_id"),
            ] {
                let count: i64 = db.query_row(
                    &format!("SELECT count(*) FROM {table} WHERE {column} IS NOT NULL"),
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(count, 0);
            }
            assert!(resumed_driver_in(db, &driver().id)?.is_none());
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
}

#[tokio::test]
async fn continuation_admission_rolls_back_suite_when_driver_insert_fails_or_is_ignored() {
    for failure in ["ABORT, 'fixture failure'", "IGNORE"] {
        let root = fixture_directory();
        let storage = open_storage(&root.path().join("metadata.sqlite"));
        let (service, instance) = continuation_service(root.path(), storage.clone()).await;
        let (source, previous, report) = continuation_history(&instance, false);
        let prepared = insert_continuation_history(&storage, &source, &previous, report.as_ref());
        storage.transaction(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER refuse_continuation BEFORE INSERT ON benchmark_drivers WHEN NEW.source_driver_id IS NOT NULL BEGIN SELECT RAISE({failure}); END;"))?;
            Ok::<_, BenchmarkError>(())
        }).unwrap();
        assert!(service.resume_driver(&previous.id).is_err());
        assert!(service.resumed_driver(&previous.id).unwrap().is_none());
        assert!(service.tasks.status().is_idle());
        storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
        storage
            .read(|db| {
                let count: i64 =
                    db.query_row("SELECT count(*) FROM benchmark_suites", [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 1);
                let count: i64 =
                    db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get(0))?;
                assert_eq!(count, 0);
                Ok::<_, BenchmarkError>(())
            })
            .unwrap();
    }
}

#[tokio::test]
async fn continuation_drivers_share_one_operational_suite_and_preserve_source_counts() {
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    let (service, instance) = continuation_service(root.path(), storage.clone()).await;
    let (source, previous, _) = continuation_history(&instance, false);
    let mut other = previous.clone();
    other.id = format!("legacy-driver-{}", "3".repeat(64));
    other.state = "complete".into();
    other.launched_run_count = other.run_count;
    other.pending_run_index = None;
    let prepared = PreparedBenchmarkImport::prepare(
        vec![source.clone()],
        vec![previous.clone(), other.clone()],
    )
    .unwrap();
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    let first = service.resume_driver(&previous.id).unwrap();
    assert!(matches!(
        service.resume_driver(&other.id),
        Err(BenchmarkError::Busy)
    ));
    assert!(service.resumed_driver(&other.id).unwrap().is_none());
    service.stop_driver(&first.id).unwrap();
    let second = service.resume_driver(&other.id).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.suite_id, second.suite_id);
    assert_eq!(second.launched_run_count, 0);
    assert_eq!(second.pending_run_index, Some(0));
    assert_eq!(
        service.resume_driver(&previous.id).unwrap().state,
        "stopped"
    );
    service.stop_driver(&second.id).unwrap();
    service
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    storage
        .read(|db| {
            let count: i64 = db.query_row(
                "SELECT count(*) FROM benchmark_suites WHERE source_suite_id=?1",
                [&source.suite_id],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1);
            let count: i64 = db.query_row(
                "SELECT count(*) FROM benchmark_drivers WHERE source_driver_id IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(count, 2);
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
}

#[tokio::test]
async fn restart_limit_continuation_response_loss_and_task_refusal_reopen_the_same_successor() {
    for refuse_task in [false, true] {
        let root = fixture_directory();
        let path = root.path().join("metadata.sqlite");
        let storage = open_storage(&path);
        let (service, instance) = continuation_service(root.path(), storage.clone()).await;
        let (source, mut previous, report) = continuation_history(&instance, true);
        previous.state = "interrupted".into();
        previous.error = Some("driver ignored after restart resume limit".into());
        let prepared = insert_continuation_history(&storage, &source, &previous, report.as_ref());
        assert_eq!(service.resume_interrupted_drivers().unwrap(), 0);
        assert!(service.can_resume_driver(&previous.id).unwrap());
        assert!(service.resumed_driver(&previous.id).unwrap().is_none());
        if refuse_task {
            service
                .tasks
                .shutdown(std::time::Duration::from_secs(2))
                .await
                .unwrap();
        }
        let result = service.resume_driver(&previous.id);
        assert_eq!(result.is_err(), refuse_task);
        drop(result);
        let accepted = service.resumed_driver(&previous.id).unwrap().unwrap();
        if refuse_task {
            assert_eq!(accepted.state, "failed");
        } else {
            service.stop_driver(&accepted.id).unwrap();
            service
                .tasks
                .shutdown(std::time::Duration::from_secs(2))
                .await
                .unwrap();
        }
        let terminal = service.driver(&accepted.id).unwrap();
        assert_eq!(service.resume_driver(&previous.id).unwrap(), terminal);
        drop(service);
        drop(storage);
        let storage = open_storage(&path);
        let reopened = self::service(root.path(), storage.clone());
        assert_eq!(
            reopened.resumed_driver(&previous.id).unwrap(),
            Some(terminal.clone())
        );
        assert_eq!(reopened.resume_driver(&previous.id).unwrap(), terminal);
        assert!(reopened.tasks.status().is_idle());
        assert_eq!(reopened.resume_interrupted_drivers().unwrap(), 0);
        assert!(!reopened.can_resume_driver(&previous.id).unwrap());
        assert!(reopened.can_resume_driver(&accepted.id).unwrap());
        storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
        assert_eq!(
            reopened
                .reports
                .get(report.as_ref().unwrap().session_id.as_str())
                .unwrap(),
            report
        );
    }
}

#[tokio::test]
async fn continuation_inherited_rows_and_private_links_are_checked_on_read_and_write() {
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    let (service, instance) = continuation_service(root.path(), storage.clone()).await;
    let (source, previous, report) = continuation_history(&instance, true);
    let prepared = insert_continuation_history(&storage, &source, &previous, report.as_ref());
    let accepted = service.resume_driver(&previous.id).unwrap();
    service.stop_driver(&accepted.id).unwrap();
    service
        .tasks
        .shutdown(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    let operational = service.suite(&accepted.suite_id).unwrap().unwrap();
    for fake_intent in [false, true] {
        let mut changed = operational.clone();
        if fake_intent {
            changed.runs[0].launch_intent = Some(uuid::Uuid::new_v4().to_string());
        } else {
            changed.runs[0].state = "failed".into();
        }
        assert!(service.save_suite(&changed).is_err());
        assert_eq!(
            service.suite(&accepted.suite_id).unwrap(),
            Some(operational.clone())
        );
    }
    let request: BenchmarkLaunchRequest =
        serde_json::from_value(serde_json::json!({"suite_id":accepted.suite_id, "run_index":0}))
            .unwrap();
    assert!(matches!(
        service.tick(request).await,
        Err(BenchmarkError::Invalid)
    ));
    storage
        .transaction(|tx| {
            tx.execute(
                "UPDATE benchmark_suites SET source_suite_id=NULL WHERE suite_id=?1",
                [&accepted.suite_id],
            )?;
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    assert!(service.suite(&accepted.suite_id).is_err());
    assert!(service.driver(&accepted.id).is_err());
    assert!(service.resumed_driver(&previous.id).is_err());
    storage
        .transaction(|tx| {
            tx.execute(
                "UPDATE benchmark_suites SET source_suite_id=?1 WHERE suite_id=?2",
                params![source.suite_id, accepted.suite_id],
            )?;
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    assert_eq!(
        service.suite(&accepted.suite_id).unwrap(),
        Some(operational)
    );
}

#[tokio::test]
async fn continuation_launch_preparation_refusal_keeps_actual_pending_summary() {
    let root = fixture_directory();
    let storage = open_storage(&root.path().join("metadata.sqlite"));
    let (service, instance) = continuation_service(root.path(), storage.clone()).await;
    let (source, previous, _) = continuation_history(&instance, false);
    let prepared = insert_continuation_history(&storage, &source, &previous, None);
    let accepted = service.resume_driver(&previous.id).unwrap();
    // The real launch owner refuses this fixture's missing selected account.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !service.tasks.status().is_idle() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let failed = service.driver(&accepted.id).unwrap();
    let operational = service.suite(&accepted.suite_id).unwrap().unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.pending_run_index, Some(1));
    assert_eq!(failed.launched_run_count, 1);
    assert_eq!(operational.runs[0].state, "failed");
    assert!(operational.runs[0].session_id.is_some());
    assert_eq!(operational.runs[1].state, "pending");
    assert!(matches!(
        service
            .launches
            .intent(operational.runs[0].launch_intent.as_deref().unwrap())
            .unwrap(),
        Some(LaunchIntentStatus::Rejected { .. })
    ));
    assert!(service.sessions.sessions().is_empty());
    assert_eq!(service.resume_driver(&previous.id).unwrap(), failed);
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
}

#[tokio::test]
async fn observed_settlement_without_report_blocks_benchmarks_without_live_process_claims() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let (service, instance) = continuation_service(root.path(), storage.clone()).await;
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

#[tokio::test]
async fn reopened_history_refuses_every_mutation_without_scheduling_or_intents() {
    let root = fixture_directory();
    let path = root.path().join("metadata.sqlite");
    let storage = open_storage(&path);
    let prepared = PreparedBenchmarkImport::prepare(vec![suite()], vec![driver()]).unwrap();
    storage.transaction(|tx| prepared.insert_in(tx)).unwrap();
    drop(storage);
    let storage = open_storage(&path);
    let service = service(root.path(), storage.clone());
    assert_eq!(service.suite(&suite().suite_id).unwrap(), Some(suite()));
    assert_eq!(service.driver(&driver().id).unwrap(), driver());
    assert_eq!(service.drivers().unwrap(), vec![driver()]);
    assert_eq!(service.resume_interrupted_drivers().unwrap(), 0);
    let input: BenchmarkLaunchRequest =
        serde_json::from_value(serde_json::json!({"suite_id":suite().suite_id})).unwrap();
    assert!(matches!(
        service.ensure_suite(&input),
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.tick(input.clone()).await,
        Err(BenchmarkError::Invalid)
    ));
    let mut rerun = input.clone();
    rerun.run_index = Some(5);
    assert!(matches!(
        service.tick(rerun).await,
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.start_driver(input.clone()).await,
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.launch(input.clone()).await,
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.stop_driver(&driver().id),
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.resume_driver(&driver().id),
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.run_driver(driver(), input),
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.save_suite(&suite()),
        Err(BenchmarkError::Invalid)
    ));
    assert!(matches!(
        service.save_driver(&driver()),
        Err(BenchmarkError::Invalid)
    ));
    storage.transaction(|tx| prepared.verify_in(tx)).unwrap();
    assert!(service.sessions.sessions().is_empty());
    assert!(service.tasks.status().is_idle());
    assert!(
        service
            .gates
            .lock()
            .unwrap()
            .values()
            .all(|gate| gate.try_lock().is_ok())
    );
    storage
        .read(|db| {
            let count: i64 =
                db.query_row("SELECT count(*) FROM launch_intents", [], |row| row.get(0))?;
            assert_eq!(count, 0);
            Ok::<_, BenchmarkError>(())
        })
        .unwrap();
}
