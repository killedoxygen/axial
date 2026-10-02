use super::*;
use crate::{import::tests::Fixture, launch::reports::LaunchReportStore, storage::MetadataStore};
use serde_json::{Value, json};
use std::{cell::Cell, fs, sync::Arc};

const INSTANCE: &str = "0000000000000001";
const SECOND: &str = "0000000000000002";
const SUITE: &str = "suite-dev-0000000000000001";
const DRIVER: &str = "benchmark-suite-driver-0000000000000001";

thread_local! {
    static REPORT_PREPARATIONS: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn record_preparation() {
    REPORT_PREPARATIONS.with(|count| count.set(count.get() + 1));
}

fn report() -> Value {
    json!({
        "schema":"axial.launch.proof", "schema_version":3,
        "session_id":"session-a", "instance_id":INSTANCE, "version_id":"1.21.1",
        "launched_at":"2026-01-01T00:00:00.000Z", "recorded_at":"2026-01-01T00:00:02.000Z",
        "outcome":"exited", "session_outcome":{"reason":"clean_exit","kind":"clean","summary":"Minecraft exited cleanly."},
        "scenario":{"scenario_id":"vanilla_launch","performance_mode":"vanilla","requested_memory_mb":1024,"version_id":"1.21.1"},
        "device":{"tier":"mid","total_memory_mb":8192,"cpu_threads":8},
        "exit_code":0,"boot_duration_ms":1000,
        "stages":[{"stage":"starting","label":"Starting process","started_at_ms":10,"ended_at_ms":510,"duration_ms":500,
            "result":"complete","warnings":[],"fallback_reason":null,"evidence":[
                {"id":"command_prepared","system":"execution","summary":"Command prepared","details":["arg_count:3"]},
                {"id":"guardian_launch_safety_decision","system":"guardian","summary":"Launch allowed","details":[]}
            ]}]
    })
}

fn write(fixture: &Fixture, name: &str, report: &Value) {
    let path = fixture.baseline.join("benchmarks/launch").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(report).unwrap()).unwrap();
}

fn benchmark_report() -> Value {
    let mut value = report();
    value["scenario"]["benchmark_profile"] = json!("vanilla_baseline");
    value["scenario"]["benchmark_run_type"] = json!("coldish");
    value["scenario"]["benchmark_mode"] = json!("development");
    value["scenario"]["benchmark_id"] = json!("benchmark-0000000000000001");
    value
}

fn suite() -> Value {
    json!({"schema":"axial.launch.benchmark.suite","schema_version":2,
        "suite_id":SUITE,"instance_id":INSTANCE,"mode":"development",
        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z",
        "runs":[{"run_index":0,"profile":"vanilla_baseline","run_type":"coldish","target_id":"",
            "benchmark_id":"benchmark-0000000000000001","session_id":"session-a",
            "launched_at":"2026-01-01T00:00:00Z","state":"exited"}]})
}

fn driver() -> Value {
    json!({"id":DRIVER,"suite_id":SUITE,"mode":"development","state":"stopped",
        "interval_ms":30000,"run_count":1,"launched_run_count":1,"pending_run_index":null,
        "active_session_id":null,"last_run_index":0,"last_session_id":"session-a",
        "error":"Stopped by the user","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z"})
}

fn write_benchmark(fixture: &Fixture, directory: &str, id: &str, value: &Value) {
    let path = fixture
        .baseline
        .join("benchmarks")
        .join(directory)
        .join(format!("{id}.json"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn write_benchmark_history(fixture: &Fixture, report: &Value, suite: &Value, driver: &Value) {
    write(fixture, "session-a.json", report);
    write_benchmark(fixture, "suites", SUITE, suite);
    write_benchmark(fixture, "suite-drivers", DRIVER, driver);
}

fn converted(value: Value) -> ImportResult<LaunchProofRecord> {
    let fixture = Fixture::new();
    write(&fixture, "session-a.json", &value);
    let inventory = fixture.capture();
    let prepared = prepare_history(&inventory)?.for_instance(INSTANCE)?;
    Ok(prepared.records[0].clone())
}

fn two_instances(fixture: &Fixture) {
    let path = fixture.baseline.join("instances.json");
    let mut registry: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut second = registry["instances"][0].clone();
    second["id"] = json!(SECOND);
    second["name"] = json!("Second");
    registry["instances"].as_array_mut().unwrap().push(second);
    fs::write(path, serde_json::to_vec(&registry).unwrap()).unwrap();
    fs::create_dir(fixture.baseline.join("instances").join(SECOND)).unwrap();
}

#[test]
fn preview_prepares_history_once_and_import_retains_only_selected_reports() {
    let fixture = Fixture::new();
    two_instances(&fixture);
    write(&fixture, "session-a.json", &report());
    let mut second = report();
    second["session_id"] = json!("session-b");
    second["instance_id"] = json!(SECOND);
    write(&fixture, "session-b.json", &second);

    REPORT_PREPARATIONS.with(|count| count.set(0));
    let inventory = Arc::new(fixture.capture());
    assert_eq!(REPORT_PREPARATIONS.with(Cell::get), 2);
    let preview = inventory.preview();
    assert_eq!(preview.instances.len(), 2);
    assert!(
        preview
            .instances
            .iter()
            .all(|row| row.ordinary_import_available)
    );

    let metadata = Arc::new(MetadataStore::in_memory().unwrap());
    let reports = LaunchReportStore::new(metadata.clone()).unwrap();
    for (legacy_id, session_id) in [(INSTANCE, "session-a"), (SECOND, "session-b")] {
        let input = inventory
            .prepare_instance(&preview.fingerprint, legacy_id)
            .unwrap();
        let destination = InstanceId::new();
        let prepared = input.bind_history(&destination).unwrap();
        metadata
            .transaction(|tx| prepared.reports.insert_in(tx))
            .unwrap();
        let saved = reports
            .list_recent(25)
            .unwrap()
            .into_iter()
            .filter(|report| report.instance_id == destination.as_str())
            .collect::<Vec<_>>();
        assert_eq!(saved.len(), 1);
        assert_eq!(
            saved[0].session_id,
            imported_id(&inventory.source_identity().unwrap(), session_id)
        );
    }

    second["boot_duration_ms"] = json!(2000);
    write(&fixture, "session-b.json", &second);
    assert!(matches!(
        inventory.prepare_instance(&preview.fingerprint, INSTANCE),
        Err(ImportError::SourceChanged)
    ));
}

#[test]
fn malformed_other_instance_history_blocks_import_without_losing_preview() {
    let fixture = Fixture::new();
    two_instances(&fixture);
    write(&fixture, "session-a.json", &report());
    let mut second = report();
    second["session_id"] = json!("session-b");
    second["instance_id"] = json!(SECOND);
    second["outcome"] = json!("running");
    write(&fixture, "session-b.json", &second);

    let inventory = Arc::new(fixture.capture());
    let preview = inventory.preview();
    assert!(preview.metadata_import_available);
    assert_eq!(preview.instances.len(), 2);
    assert!(
        preview
            .instances
            .iter()
            .all(|row| !row.ordinary_import_available)
    );
    assert!(
        preview
            .blockers
            .contains(&ImportBlocker::RetainedHistoryRequiresConversion)
    );
    assert!(matches!(
        inventory.prepare_instance(&preview.fingerprint, INSTANCE),
        Err(ImportError::InvalidData)
    ));
}

#[test]
fn archived_reports_share_publication_with_survivors_and_keep_pruned_comparisons() {
    let fixture = Fixture::new();
    write(&fixture, "session-a.json", &report());
    let mut archived = report();
    archived["session_id"] = json!("session-b");
    archived["instance_id"] = json!(SECOND);
    archived["comparison"] = json!({"baseline_session_id":"session-pruned","baseline_recorded_at":"2025-12-31T00:00:00.000Z",
        "baseline":{"performance_mode":"vanilla","version_id":"1.21.1","requested_memory_mb":1024,"device_tier":"mid"},
        "matched_sample_count":3,"metric_name":"boot_duration_ms","current_value_ms":1000,"baseline_value_ms":2000,"delta_ms":-1000,"delta_percent":-50.0});
    write(&fixture, "session-b.json", &archived);
    let before = crate::import::tests::snapshot(&fixture.baseline);
    let inventory = fixture.capture();
    let source = inventory.source_identity().unwrap();
    let batch = prepare_archived_history(&inventory).unwrap().reports;
    assert_eq!(batch.completion_proof(&source).unwrap().count(), 1);
    let history = prepare_history(&inventory).unwrap();
    assert_eq!(history.supported_records().len(), 2);
    let destination = InstanceId::new();
    let bound = history
        .for_instance(INSTANCE)
        .unwrap()
        .bind_instance(&destination)
        .unwrap();
    let metadata = Arc::new(MetadataStore::in_memory().unwrap());
    let reports = LaunchReportStore::new(metadata.clone()).unwrap();
    metadata
        .transaction(|tx| bound.reports.insert_in(tx))
        .unwrap();
    let archived_id = imported_id(&source, "session-b");
    let saved = reports.get(&archived_id).unwrap().unwrap();
    assert_eq!(saved.instance_id, format!("archived-{source}-{SECOND}"));
    assert_eq!(
        saved.comparison.as_ref().unwrap().baseline_session_id,
        imported_id(&source, "session-pruned")
    );
    assert!(
        reports
            .get(&saved.comparison.as_ref().unwrap().baseline_session_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reports
            .get(&imported_id(&source, "session-a"))
            .unwrap()
            .unwrap()
            .instance_id,
        destination.as_str()
    );
    metadata.transaction(|tx| batch.insert_in(tx)).unwrap();
    assert_eq!(reports.get(&archived_id).unwrap(), Some(saved));
    assert_eq!(reports.list_recent(25).unwrap().len(), 2);
    assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
}

#[test]
fn archived_reports_require_original_registry_identity_evidence() {
    for variant in [
        "schema",
        "missing_pending",
        "missing_last",
        "null_last",
        "duplicate_outer",
        "duplicate_id_field",
        "too_many_pending",
        "dangling_selection",
    ] {
        let fixture = Fixture::new();
        let path = fixture.baseline.join("instances.json");
        let mut registry: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match variant {
            "schema" => registry["schema_version"] = json!(4),
            "missing_pending" => {
                registry
                    .as_object_mut()
                    .unwrap()
                    .remove("pending_deletions");
            }
            "missing_last" => {
                registry.as_object_mut().unwrap().remove("last_instance_id");
            }
            "null_last" => registry["last_instance_id"] = Value::Null,
            "too_many_pending" => registry["pending_deletions"] = json!([{}, {}]),
            "dangling_selection" => registry["last_instance_id"] = json!(SECOND),
            _ => {}
        }
        let mut raw = serde_json::to_string(&registry).unwrap();
        if variant == "duplicate_outer" {
            raw.insert_str(1, "\"instances\":[],");
        } else if variant == "duplicate_id_field" {
            raw = raw.replacen(
                &format!("\"id\":\"{INSTANCE}\""),
                &format!("\"id\":\"{SECOND}\",\"id\":\"{INSTANCE}\""),
                1,
            );
        }
        fs::write(path, raw).unwrap();
        let mut archived = report();
        archived["instance_id"] = json!(SECOND);
        write(&fixture, "session-a.json", &archived);
        let inventory = fixture.capture();
        let result = prepare_archived_history(&inventory);
        if variant == "dangling_selection" {
            assert_eq!(
                result
                    .unwrap()
                    .reports
                    .completion_proof(&inventory.source_identity().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        } else {
            assert!(result.is_err(), "{variant}");
            assert!(inventory.preview().metadata_import_available, "{variant}");
        }
    }
}

#[test]
fn archived_reports_do_not_waive_live_reports_suites_or_unsettled_journals() {
    for obligation in ["live_report", "archived_suite", "journal", "bad_archive"] {
        let fixture = Fixture::new();
        let mut archived = report();
        archived["instance_id"] = json!(SECOND);
        if obligation == "bad_archive" {
            archived["outcome"] = json!("running");
        }
        write(&fixture, "session-a.json", &archived);
        match obligation {
            "live_report" => {
                let mut live = report();
                live["session_id"] = json!("session-live");
                live["outcome"] = json!("running");
                write(&fixture, "session-live.json", &live);
            }
            "archived_suite" => {
                let mut archived_suite = suite();
                archived_suite["instance_id"] = json!(SECOND);
                write_benchmark(&fixture, "suites", SUITE, &archived_suite);
            }
            "journal" => {
                let mut journal = crate::import::tests::successful_install_journal();
                journal["entries"][0]["status"] = json!("Running");
                let path = fixture.baseline.join("state/operation-journals.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
            }
            _ => {}
        }
        let inventory = fixture.capture();
        assert!(prepare_history(&inventory).is_err(), "{obligation}");
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{obligation}"
        );
        let batch = prepare_archived_history(&inventory);
        if obligation == "bad_archive" {
            assert!(batch.is_err());
        } else {
            assert_eq!(
                batch
                    .unwrap()
                    .reports
                    .completion_proof(&inventory.source_identity().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        }
    }
}

#[test]
fn archived_benchmarks_co_publish_without_destination_instance_or_execution_authority() {
    for state in ["terminal", "all_pending", "mixed"] {
        let fixture = Fixture::new();
        let mut archived_report = benchmark_report();
        archived_report["instance_id"] = json!(SECOND);
        let mut archived_suite = suite();
        archived_suite["instance_id"] = json!(SECOND);
        let mut archived_driver = driver();
        if state == "all_pending" {
            archived_suite["runs"][0]["state"] = json!("pending");
            archived_suite["runs"][0]["session_id"] = Value::Null;
            archived_suite["runs"][0]["launched_at"] = Value::Null;
            archived_driver["launched_run_count"] = json!(0);
            archived_driver["last_run_index"] = Value::Null;
            archived_driver["last_session_id"] = Value::Null;
            archived_driver["pending_run_index"] = json!(0);
        } else if state == "mixed" {
            let mut pending = archived_suite["runs"][0].clone();
            pending["run_index"] = json!(1);
            pending["benchmark_id"] = json!("benchmark-0000000000000002");
            pending["state"] = json!("pending");
            pending["session_id"] = Value::Null;
            pending["launched_at"] = Value::Null;
            archived_suite["runs"].as_array_mut().unwrap().push(pending);
            archived_driver["run_count"] = json!(2);
            archived_driver["pending_run_index"] = json!(1);
        }
        write_benchmark_history(
            &fixture,
            &archived_report,
            &archived_suite,
            &archived_driver,
        );
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        let source = inventory.source_identity().unwrap();
        let archive = prepare_archived_history(&inventory).unwrap();
        let benchmark_batch = archive.benchmarks.unwrap();
        assert_eq!(
            benchmark_batch.completion_proof(&source).unwrap().count(),
            2
        );
        let history = prepare_history(&inventory).unwrap();
        assert_eq!(history.supported_records().len(), 3);
        let bound = history
            .for_instance(INSTANCE)
            .unwrap()
            .bind_instance(&InstanceId::new())
            .unwrap();
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let reports = LaunchReportStore::new(metadata.clone()).unwrap();
        metadata
            .migrate(&[
                crate::performance::benchmarks::MIGRATION,
                crate::performance::benchmarks::MIGRATION_V2,
                crate::performance::benchmarks::MIGRATION_V3,
            ])
            .unwrap();
        metadata
            .transaction(|tx| {
                bound.reports.insert_in(tx).unwrap();
                bound.benchmarks.insert_in(tx)?;
                bound.benchmarks.verify_in(tx)
            })
            .unwrap();
        let saved: BenchmarkSuiteManifest = metadata
            .read(|db| -> Result<_, crate::storage::StorageError> {
                let payload: Vec<u8> =
                    db.query_row("SELECT payload FROM benchmark_suites", [], |row| row.get(0))?;
                Ok(serde_json::from_slice(&payload).unwrap())
            })
            .unwrap();
        assert_eq!(saved.instance_id, format!("archived-{source}-{SECOND}"));
        assert!(saved.historical);
        assert!(saved.runs.iter().all(|run| run.launch_intent.is_none()));
        assert_eq!(
            reports.list_recent(10).unwrap()[0].instance_id,
            saved.instance_id
        );
        metadata
            .transaction(|tx| {
                archive.reports.insert_in(tx).unwrap();
                benchmark_batch.insert_in(tx)?;
                benchmark_batch.verify_in(tx)
            })
            .unwrap();
        let authorities: u32 = metadata.read(|db| -> Result<_, crate::storage::StorageError> {
            Ok(db.query_row("SELECT count(*) FROM benchmark_drivers WHERE request IS NOT NULL OR source_driver_id IS NOT NULL", [], |row| row.get(0))?)
        }).unwrap();
        assert_eq!(authorities, 0);
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[test]
fn archived_benchmarks_do_not_waive_unrelated_live_work_or_missing_report_batch() {
    for unrelated in ["journal", "live_suite", "bad_archived_report"] {
        let fixture = Fixture::new();
        let mut archived_report = benchmark_report();
        archived_report["instance_id"] = json!(SECOND);
        let mut archived_suite = suite();
        archived_suite["instance_id"] = json!(SECOND);
        write_benchmark_history(&fixture, &archived_report, &archived_suite, &driver());
        match unrelated {
            "journal" => {
                let mut journal = crate::import::tests::successful_install_journal();
                journal["entries"][0]["status"] = json!("Running");
                let path = fixture.baseline.join("state/operation-journals.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
            }
            "live_suite" => {
                let mut live = suite();
                live["suite_id"] = json!("suite-dev-0000000000000003");
                live["runs"][0]["state"] = json!("running");
                write_benchmark(&fixture, "suites", "suite-dev-0000000000000003", &live);
            }
            _ => {
                let mut invalid = archived_report.clone();
                invalid["session_id"] = json!("session-bad");
                invalid["outcome"] = json!("running");
                write(&fixture, "session-bad.json", &invalid);
            }
        }
        let inventory = fixture.capture();
        assert!(prepare_history(&inventory).is_err(), "{unrelated}");
        assert!(!inventory.preview().instances[0].ordinary_import_available);
        let archive = prepare_archived_history(&inventory);
        if unrelated == "bad_archived_report" {
            assert!(archive.is_err());
        } else {
            assert_eq!(
                archive
                    .unwrap()
                    .benchmarks
                    .unwrap()
                    .completion_proof(&inventory.source_identity().unwrap())
                    .unwrap()
                    .count(),
                2
            );
        }
    }
}

#[test]
fn detached_driver_import_requires_authoritative_absence_not_bad_parent_or_queued_handoff() {
    for case in [
        "absent",
        "queued",
        "nonterminal",
        "active_session",
        "invalid_suite_id",
        "bad_last_index",
        "bad_last_session",
        "schema",
        "malformed",
        "directory",
        "namespace_file",
        "live_parent",
        "journal",
    ] {
        let fixture = Fixture::new();
        let mut status = driver();
        match case {
            "queued" => {
                status["state"] = json!("interrupted");
                status["error"] = json!("driver automatic resume queued after restart");
            }
            "nonterminal" => status["state"] = json!("scheduled"),
            "active_session" => status["active_session_id"] = json!("session-a"),
            "invalid_suite_id" => status["suite_id"] = json!("not-a-suite"),
            "bad_last_index" => status["last_run_index"] = json!(64),
            "bad_last_session" => status["last_session_id"] = json!("../private/session"),
            _ => {}
        }
        write_benchmark(&fixture, "suite-drivers", DRIVER, &status);
        match case {
            "schema" => {
                let mut parent = suite();
                parent["instance_id"] = json!(SECOND);
                parent["schema_version"] = json!(77);
                write_benchmark(&fixture, "suites", SUITE, &parent);
            }
            "live_parent" => write_benchmark(&fixture, "suites", SUITE, &suite()),
            "journal" => {
                let mut journal = crate::import::tests::successful_install_journal();
                journal["entries"][0]["status"] = json!("Running");
                let path = fixture.baseline.join("state/operation-journals.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
            }
            "malformed" => {
                let path = fixture.baseline.join("benchmarks/suites");
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join(format!("{SUITE}.json")), b"{").unwrap();
            }
            "directory" => fs::create_dir_all(
                fixture
                    .baseline
                    .join("benchmarks/suites")
                    .join(format!("{SUITE}.json")),
            )
            .unwrap(),
            "namespace_file" => fs::write(
                fixture.baseline.join("benchmarks/suites"),
                b"not a suite directory",
            )
            .unwrap(),
            _ => {}
        }
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        let archived = prepare_archived_history(&inventory).unwrap();
        if matches!(case, "absent" | "live_parent" | "journal") {
            assert_eq!(
                archived
                    .benchmarks
                    .unwrap()
                    .completion_proof(&inventory.source_identity().unwrap())
                    .unwrap()
                    .count(),
                usize::from(case != "live_parent")
            );
            assert_eq!(
                prepare_history(&inventory).is_ok(),
                case == "absent",
                "{case}"
            );
            assert_eq!(
                inventory.preview().instances[0].ordinary_import_available,
                case == "absent",
                "{case}"
            );
        } else {
            assert!(archived.benchmarks.is_none(), "{case}");
            assert!(prepare_history(&inventory).is_err(), "{case}");
        }
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[cfg(unix)]
#[test]
fn detached_driver_does_not_treat_unsafe_suite_namespace_as_pruned() {
    for case in ["symlink_parent", "symlink_namespace", "hardlink_parent"] {
        let fixture = Fixture::new();
        write_benchmark(&fixture, "suite-drivers", DRIVER, &driver());
        let external = fixture
            .baseline
            .parent()
            .unwrap()
            .join("private-suite.json");
        let bytes = serde_json::to_vec(&suite()).unwrap();
        fs::write(&external, &bytes).unwrap();
        let namespace = fixture.baseline.join("benchmarks/suites");
        if case == "symlink_namespace" {
            std::os::unix::fs::symlink(&external, &namespace).unwrap();
        } else {
            fs::create_dir_all(&namespace).unwrap();
            let parent = namespace.join(format!("{SUITE}.json"));
            if case == "symlink_parent" {
                std::os::unix::fs::symlink(&external, parent).unwrap();
            } else {
                fs::hard_link(&external, parent).unwrap();
            }
        }
        let before = crate::import::tests::snapshot(&fixture.baseline);
        if case == "hardlink_parent" {
            assert!(matches!(fixture.try_capture(), Err(ImportError::Io(_))));
            assert_eq!(fs::read(&external).unwrap(), bytes);
            assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
            continue;
        }
        let inventory = fixture.capture();
        assert!(
            prepare_archived_history(&inventory)
                .unwrap()
                .benchmarks
                .is_none(),
            "{case}"
        );
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{case}"
        );
        assert!(prepare_history(&inventory).is_err(), "{case}");
        assert_eq!(fs::read(&external).unwrap(), bytes);
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[test]
fn detached_driver_does_not_treat_case_aliased_suite_namespace_as_pruned() {
    for alias in [
        "benchmarks/Suites",
        "Benchmarks/suites",
        "Benchmarks/Suites",
    ] {
        let fixture = Fixture::new();
        let parent = fixture.baseline.join(alias);
        fs::create_dir_all(&parent).unwrap();
        fs::write(
            parent.join(format!("{SUITE}.json")),
            serde_json::to_vec(&suite()).unwrap(),
        )
        .unwrap();
        write_benchmark(&fixture, "suite-drivers", DRIVER, &driver());
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        assert!(
            inventory
                .directory_names()
                .any(|path| path == format!("profile/{alias}"))
        );
        assert!(
            prepare_archived_history(&inventory)
                .unwrap()
                .benchmarks
                .is_none(),
            "{alias}"
        );
        assert!(prepare_history(&inventory).is_err(), "{alias}");
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{alias}"
        );
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[test]
fn detached_driver_does_not_acknowledge_case_aliased_driver_folder_as_empty() {
    let fixture = Fixture::new();
    write_benchmark(&fixture, "Suite-drivers", DRIVER, &driver());
    let before = crate::import::tests::snapshot(&fixture.baseline);
    let inventory = fixture.capture();
    assert!(
        inventory
            .directory_names()
            .any(|path| path == "profile/benchmarks/Suite-drivers")
    );
    assert!(
        !inventory
            .file_manifests()
            .any(|file| file.relative.starts_with(DRIVER_PREFIX))
    );
    assert!(
        prepare_archived_history(&inventory)
            .unwrap()
            .benchmarks
            .is_none()
    );
    assert!(prepare_history(&inventory).is_err());
    assert!(!inventory.preview().instances[0].ordinary_import_available);
    assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
}

#[test]
fn detached_driver_co_publication_is_identical_in_both_import_orders() {
    for metadata_first in [false, true] {
        let fixture = Fixture::new();
        write_benchmark(&fixture, "suite-drivers", DRIVER, &driver());
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        let source = inventory.source_identity().unwrap();
        let metadata_batch = prepare_archived_history(&inventory)
            .unwrap()
            .benchmarks
            .unwrap();
        let bound = prepare_history(&inventory)
            .unwrap()
            .for_instance(INSTANCE)
            .unwrap()
            .bind_instance(&InstanceId::new())
            .unwrap();
        let metadata = MetadataStore::in_memory().unwrap();
        metadata
            .migrate(&[
                crate::performance::benchmarks::MIGRATION,
                crate::performance::benchmarks::MIGRATION_V2,
                crate::performance::benchmarks::MIGRATION_V3,
            ])
            .unwrap();
        let batches = if metadata_first {
            [&*metadata_batch, &bound.benchmarks]
        } else {
            [&bound.benchmarks, &*metadata_batch]
        };
        for batch in batches {
            metadata.transaction(|tx| batch.insert_in(tx)).unwrap();
            metadata.read(|db| batch.verify_in(db)).unwrap();
        }
        let saved: BenchmarkSuiteDriverStatus = metadata
            .read(|db| -> Result<_, crate::storage::StorageError> {
                let bytes: Vec<u8> =
                    db.query_row("SELECT payload FROM benchmark_drivers", [], |row| {
                        row.get(0)
                    })?;
                Ok(serde_json::from_slice(&bytes).unwrap())
            })
            .unwrap();
        assert_eq!(saved.id, imported_benchmark_id(&source, "driver", DRIVER));
        assert_eq!(
            saved.suite_id,
            imported_benchmark_id(&source, "suite", SUITE)
        );
        assert_eq!(
            saved.last_session_id.as_deref(),
            Some(imported_id(&source, "session-a").as_str())
        );
        assert!(saved.historical);
        let no_authority = metadata.read(|db| -> Result<_, crate::storage::StorageError> {
            Ok(db.query_row("SELECT (SELECT count(*) FROM benchmark_suites),(SELECT count(*) FROM benchmark_drivers WHERE request IS NOT NULL OR source_driver_id IS NOT NULL)", [], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)))?)
        }).unwrap();
        assert_eq!(no_authority, (0, 0));
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[test]
fn archived_content_co_publication_preserves_exact_history_in_both_orders() {
    for metadata_first in [false, true] {
        let fixture = Fixture::new();
        let mut journal = crate::import::tests::successful_install_journal();
        journal["entries"].as_array_mut().unwrap().push(
            crate::import::tests::cancelled_content_initialization_journal()["entries"][0].clone(),
        );
        journal["next_sequence"] = json!(15);
        for entry in journal["entries"].as_array_mut().unwrap() {
            for target in entry["targets"].as_array_mut().unwrap() {
                if target["kind"] == "Instance" {
                    target["id"] = json!(SECOND);
                }
            }
        }
        let path = fixture.baseline.join("state/operation-journals.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        let source = inventory.source_identity().unwrap();
        let archive = prepare_archived_content_history(&inventory).unwrap();
        let global = prepare_global_install_history(&inventory).unwrap();
        let bound = prepare_history(&inventory)
            .unwrap()
            .for_instance(INSTANCE)
            .unwrap()
            .bind_instance(&InstanceId::new())
            .unwrap();
        assert!(inventory.preview().instances[0].ordinary_import_available);
        let store = MetadataStore::in_memory().unwrap();
        store
            .migrate(&[crate::install::history::MIGRATION])
            .unwrap();
        let mut metadata = global.clone();
        metadata.append(&archive).unwrap();
        let batches = if metadata_first {
            [&metadata, &bound.installs]
        } else {
            [&bound.installs, &metadata]
        };
        let mut original = None;
        for batch in batches {
            store.transaction(|tx| batch.insert_in(tx)).unwrap();
            store.read(|db| batch.verify_in(db)).unwrap();
            let saved = store
                .read(
                    |db| -> Result<
                        Vec<(String, Option<String>, Vec<u8>)>,
                        crate::storage::StorageError,
                    > {
                        Ok(db
                            .prepare(
                                "SELECT id,instance_id,payload FROM install_history ORDER BY id",
                            )?
                            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                            .collect::<Result<_, _>>()?)
                    },
                )
                .unwrap();
            if let Some(previous) = &original {
                assert_eq!(&saved, previous);
            } else {
                original = Some(saved);
            }
        }
        let proof = archive.archived_completion_proof(&source).unwrap();
        assert_eq!(proof.count(), 2);
        store
            .read(|db| {
                crate::install::history::read_completed_in(db, &source, None, Some(&proof), None)
            })
            .unwrap();
        for (_, instance, bytes) in original.unwrap() {
            let saved: Value = serde_json::from_slice(&bytes).unwrap();
            let original = journal["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["operation_id"] == saved["source"]["operation_id"])
                .unwrap();
            assert_eq!(&saved["source"], original);
            assert_eq!(
                instance,
                (original["command"] == "ModifyInstanceContent")
                    .then(|| format!("archived-{source}-{SECOND}"))
            );
        }
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[test]
fn archived_content_rules_projection_preserves_rules_and_refuses_unsettled_content() {
    for case in [
        "success",
        "initialization_cancelled",
        "generic_failed",
        "running",
    ] {
        let fixture = Fixture::new();
        let mut journal = crate::import::tests::terminal_rules_journal();
        let mut entry = if case == "initialization_cancelled" {
            crate::import::tests::cancelled_content_initialization_journal()["entries"][0].clone()
        } else {
            crate::import::tests::successful_install_journal()["entries"][2].clone()
        };
        entry["targets"][1]["id"] = json!(SECOND);
        match case {
            "generic_failed" => {
                entry["status"] = json!("Failed");
                entry["outcome"] = json!("Failed");
                entry["failure_point"] = json!("install_progress_error");
            }
            "running" => {
                entry["status"] = json!("Running");
                entry["outcome"] = Value::Null;
            }
            _ => {}
        }
        journal["entries"].as_array_mut().unwrap().push(entry);
        journal["next_sequence"] = json!(15);
        let path = fixture.baseline.join("state/operation-journals.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let inventory = fixture.capture();
        let rules = prepare_rules_history(&inventory);
        let content = prepare_archived_content_history(&inventory);
        if matches!(case, "success" | "initialization_cancelled") {
            assert_eq!(rules.unwrap().len(), 4, "{case}");
            assert_eq!(
                content
                    .unwrap()
                    .archived_completion_proof(&inventory.source_identity().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        } else {
            assert!(rules.is_err(), "{case}");
            assert!(content.is_err(), "{case}");
            assert!(prepare_history(&inventory).is_err(), "{case}");
        }
    }
}

#[test]
fn archived_performance_co_publication_preserves_exact_operations_in_both_orders() {
    for metadata_first in [false, true] {
        let fixture = Fixture::new();
        let mut journal = crate::import::tests::terminal_performance_journal();
        for entry in journal["entries"].as_array_mut().unwrap() {
            entry["intent"]["intent"]["instance_id"] = json!(SECOND);
            entry["targets"][0]["id"] = json!(SECOND);
        }
        let path = fixture.baseline.join("state/operation-journals.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        let source = inventory.source_identity().unwrap();
        let archive = prepare_archived_operations(&inventory).unwrap();
        let bound = prepare_history(&inventory)
            .unwrap()
            .for_instance(INSTANCE)
            .unwrap()
            .bind_instance(&InstanceId::new())
            .unwrap();
        assert!(inventory.preview().instances[0].ordinary_import_available);
        let metadata = MetadataStore::in_memory().unwrap();
        metadata
            .migrate(&[
                crate::performance::mutation::MIGRATION,
                crate::performance::mutation::MIGRATION_V2,
            ])
            .unwrap();
        let batches = if metadata_first {
            [&archive, &bound.operations]
        } else {
            [&bound.operations, &archive]
        };
        for batch in batches {
            metadata.transaction(|tx| batch.insert_in(tx)).unwrap();
            metadata.read(|db| batch.verify_in(db)).unwrap();
        }
        let proof = archive.completion_proof(&source).unwrap();
        assert_eq!(proof.count(), 6);
        metadata.read(|db| proof.verify_in(db, &source)).unwrap();
        let saved = metadata.read(|db| -> Result<Vec<(String, String, Vec<u8>)>, crate::storage::StorageError> {
            Ok(db.prepare("SELECT instance_id,state,payload FROM performance_commands ORDER BY id")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect::<Result<_, _>>()?)
        }).unwrap();
        assert_eq!(saved.len(), 6);
        for (instance, state, bytes) in saved {
            assert_eq!(instance, format!("archived-{source}-{SECOND}"));
            assert_eq!(state, "historical");
            let record: Value = serde_json::from_slice(&bytes).unwrap();
            let original = journal["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["operation_id"] == record["evidence"]["operation_id"])
                .unwrap();
            assert_eq!(record["evidence"]["intent"], original["intent"]["intent"]);
            assert_eq!(
                record["evidence"]["terminal"],
                original["intent"]["phase"]["terminal"]
            );
        }
        assert_eq!(crate::import::tests::snapshot(&fixture.baseline), before);
    }
}

#[test]
fn archived_performance_does_not_waive_other_journal_families_or_unresolved_effects() {
    for case in ["failed_content", "running_install", "unknown_command"] {
        let fixture = Fixture::new();
        let mut journal = crate::import::tests::terminal_performance_journal();
        for entry in journal["entries"].as_array_mut().unwrap() {
            entry["intent"]["intent"]["instance_id"] = json!(SECOND);
            entry["targets"][0]["id"] = json!(SECOND);
        }
        let other = crate::import::tests::successful_install_journal();
        let mut entry = other["entries"][if case == "failed_content" { 2 } else { 0 }].clone();
        match case {
            "failed_content" => {
                entry["targets"][1]["id"] = json!(SECOND);
                entry["status"] = json!("Failed");
                entry["outcome"] = json!("Failed");
                entry["failure_point"] = json!("install_progress_error");
            }
            "running_install" => entry["status"] = json!("Running"),
            _ => entry["command"] = json!("UnknownCommand"),
        }
        journal["entries"].as_array_mut().unwrap().push(entry);
        journal["next_sequence"] = other["next_sequence"].clone();
        let path = fixture.baseline.join("state/operation-journals.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let inventory = fixture.capture();
        let archive = prepare_archived_operations(&inventory).unwrap();
        assert_eq!(
            archive
                .completion_proof(&inventory.source_identity().unwrap())
                .unwrap()
                .count(),
            6
        );
        assert!(prepare_history(&inventory).is_err(), "{case}");
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{case}"
        );
    }
}

#[test]
fn terminal_conversion_preserves_neutral_fields_comparison_and_source_bytes() {
    let fixture = Fixture::new();
    let mut value = report();
    value["pid"] = json!(4321);
    value["priority"] = json!({"start_mode":"normal","start_error":"Priority unavailable","promotion":"promoted","promotion_error":"Promotion unavailable"});
    value["failure_class"] = json!("out_of_memory");
    value["failure_detail"] = json!("Historical allocation evidence");
    value["guardian"] = json!({"decision":"allow"});
    value["healing"] = json!({"decision":"none"});
    value["resource_budget"] = json!({"host_total_memory_mb":8192,"host_available_memory_mb":4096,"host_used_memory_mb":4096,
        "host_cpu_threads":8,"host_cpu_load_1m_x100":100,"host_cpu_load_5m_x100":90,"host_cpu_load_15m_x100":80,
        "launcher_process_memory_mb":128,"active_session_count":1,"active_install_count":0,"active_memory_allocation_mb":1024,
        "requested_memory_mb":1024,"estimated_remaining_memory_mb":3072,"memory_headroom_mb":512,"memory_pressure":false,
        "cpu_pressure":false,"install_pressure":false,"launch_disk_available_mb":10000,"launch_disk_headroom_mb":1000,"disk_pressure":false});
    value["comparison"] = json!({"baseline_session_id":"session-before","baseline_recorded_at":"2025-12-31T00:00:00.000Z",
        "baseline":{"performance_mode":"vanilla","version_id":"1.21.1","requested_memory_mb":1024,"device_tier":"mid"},
        "matched_sample_count":3,"metric_name":"boot_duration_ms","current_value_ms":1000,"baseline_value_ms":2000,"delta_ms":-1000,"delta_percent":-50.0});
    write(&fixture, "session-a.json", &value);
    let before = crate::import::tests::snapshot(&fixture.baseline);
    let inventory = fixture.capture();
    let history = prepare_history(&inventory).unwrap();
    assert_eq!(
        history.supported_records(),
        &BTreeSet::from(["profile/benchmarks/launch/session-a.json".into()])
    );
    let metadata = Arc::new(MetadataStore::in_memory().unwrap());
    let reports = LaunchReportStore::new(metadata.clone()).unwrap();
    let destination = InstanceId::new();
    let prepared = history
        .for_instance(INSTANCE)
        .unwrap()
        .bind_instance(&destination)
        .unwrap();
    metadata
        .transaction(|tx| prepared.reports.insert_in(tx))
        .unwrap();
    let saved = reports.list_recent(25).unwrap().pop().unwrap();
    assert_eq!(saved.instance_id, destination.as_str());
    assert!(saved.session_id.starts_with("legacy-"));
    assert!(saved.session_id.parse::<uuid::Uuid>().is_err());
    assert_eq!(saved.boot_duration_ms, Some(1000));
    assert_eq!(
        saved
            .resource_budget
            .as_ref()
            .unwrap()
            .estimated_remaining_memory_mb,
        Some(3072)
    );
    assert_eq!(
        saved.session_outcome.failure_class,
        Some(FailureClass::OutOfMemory)
    );
    assert_eq!(saved.comparison.as_ref().unwrap().delta_percent, -50.0);
    assert_eq!(saved.comparison.as_ref().unwrap().matched_sample_count, 3);
    assert_eq!(
        saved.comparison.as_ref().unwrap().baseline_session_id,
        imported_id(&inventory.source_identity().unwrap(), "session-before")
    );
    let retained = serde_json::to_string(&saved).unwrap();
    for evidence in [
        "Priority unavailable",
        "Promotion unavailable",
        "Historical allocation evidence",
        "4321",
        "arg_count:3",
    ] {
        assert!(retained.contains(evidence), "lost {evidence}");
    }
    assert!(!retained.contains("guardian"));
    assert!(!retained.contains("healing"));
    assert_eq!(total_duration(&saved.stages), Some(500));
    assert!(saved.logs.is_empty());
    assert_eq!(saved.logs_dropped, 0);
    assert_eq!(before, crate::import::tests::snapshot(&fixture.baseline));
}

#[test]
fn missing_terminal_classification_stays_unknown_and_watchdog_reason_is_retained() {
    let mut value = report();
    value.as_object_mut().unwrap().remove("session_outcome");
    let unknown = converted(value).unwrap();
    assert_eq!(unknown.session_outcome.kind, SessionOutcomeKind::Unknown);
    assert_eq!(
        unknown.session_outcome.reason,
        SessionExitReason::UnknownExit
    );
    assert_eq!(unknown.outcome, "unknown");
    assert_eq!(unknown.exit_code, Some(0));
    let mut value = report();
    value["outcome"] = json!("failed");
    value["session_outcome"] = json!({"reason":"watchdog_killed","kind":"failed","summary":"Minecraft did not finish startup in time."});
    let watchdog = converted(value).unwrap();
    assert_eq!(
        watchdog.session_outcome.reason,
        SessionExitReason::StartupStalled
    );
    assert!(
        serde_json::to_string(&watchdog)
            .unwrap()
            .contains("watchdog_killed")
    );
}

#[test]
fn malformed_nonterminal_ambiguous_and_lossy_reports_remain_blocked() {
    for (key, value) in [
        ("schema_version", json!(4)),
        ("outcome", json!("running")),
        ("outcome", json!("degraded")),
        ("outcome", json!("stopped")),
        ("instance_id", json!("not-an-instance")),
        ("session_id", json!("another-session")),
        ("launched_at", json!("2026-01-01T00:00:00Z")),
        ("recorded_at", json!("2025-01-01T00:00:00.000Z")),
        ("failure_class", json!("future_class")),
        ("unknown_field", json!(true)),
    ] {
        let mut candidate = report();
        candidate[key] = value;
        assert!(converted(candidate).is_err(), "accepted {key}");
    }
    let mut incoherent = report();
    incoherent["stages"][0]["duration_ms"] = json!(1);
    assert!(converted(incoherent).is_err());
    let mut path = report();
    path["failure_detail"] = json!("relative/private-file");
    assert!(converted(path).is_err());
    let mut full = report();
    full["stages"] = Value::Array(vec![full["stages"][0].clone(); 32]);
    assert!(converted(full).is_err());
    let mut outcome = report();
    outcome["session_outcome"]["kind"] = json!("failed");
    assert!(converted(outcome).is_err());
    let mut unknown = report();
    unknown["outcome"] = json!("unknown");
    unknown.as_object_mut().unwrap().remove("session_outcome");
    assert!(converted(unknown).is_err());
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.baseline.join("benchmarks/suites")).unwrap();
    fs::write(fixture.baseline.join("benchmarks/suites/old.json"), b"{}").unwrap();
    assert!(prepare_history(&fixture.capture()).is_err());
}

#[test]
fn source_scope_is_stable_without_aliasing_another_profile() {
    assert_eq!(
        imported_id("source-a", "session-a"),
        imported_id("source-a", "session-a")
    );
    assert_ne!(
        imported_id("source-a", "session-a"),
        imported_id("source-b", "session-a")
    );
    assert_ne!(imported_id("a", "bc"), imported_id("ab", "c"));
    assert_ne!(
        imported_benchmark_id("source-a", "suite", SUITE),
        imported_benchmark_id("source-b", "suite", SUITE)
    );
    assert_ne!(
        imported_benchmark_id("source-a", "suite", "same"),
        imported_benchmark_id("source-a", "driver", "same")
    );
}

#[test]
fn terminal_benchmark_history_preserves_exact_read_only_records_and_source() {
    use crate::performance::benchmarks::{BenchmarkError, MIGRATION, MIGRATION_V2, MIGRATION_V3};
    for (state, error) in [
        ("stopped", "Stopped by the user"),
        (
            "interrupted",
            "driver automatic resume queued after restart",
        ),
        ("interrupted", "driver ignored after restart resume limit"),
    ] {
        let fixture = Fixture::new();
        let mut status = driver();
        status["state"] = json!(state);
        status["error"] = json!(error);
        write_benchmark_history(&fixture, &benchmark_report(), &suite(), &status);
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(preview.instances[0].ordinary_import_available);
        assert!(!preview.cutover_available);
        assert!(
            preview
                .blockers
                .contains(&ImportBlocker::RetainedHistoryRequiresConversion)
        );
        let prepared = inventory
            .prepare_instance(&preview.fingerprint, INSTANCE)
            .unwrap();
        let destination = InstanceId::new();
        let bound = prepared.bind_history(&destination).unwrap();
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        for _ in 0..2 {
            let metadata =
                Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
            metadata
                .migrate(&[MIGRATION, MIGRATION_V2, MIGRATION_V3])
                .unwrap();
            let reports = LaunchReportStore::new(metadata.clone()).unwrap();
            metadata
                .transaction(|tx| -> Result<(), BenchmarkError> {
                    bound
                        .reports
                        .insert_in(tx)
                        .map_err(|_| BenchmarkError::Unavailable)?;
                    bound.benchmarks.insert_in(tx)
                })
                .unwrap();
            let (saved_suite, saved_driver, request) = metadata
                .read(|db| -> Result<_, crate::storage::StorageError> {
                    let suite: Vec<u8> =
                        db.query_row("SELECT payload FROM benchmark_suites", [], |row| row.get(0))?;
                    let (driver, request): (Vec<u8>, Option<Vec<u8>>) =
                        db.query_row("SELECT payload,request FROM benchmark_drivers", [], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })?;
                    assert_eq!(
                        db.query_row("SELECT count(*) FROM benchmark_suites", [], |row| row
                            .get::<_, usize>(0))?,
                        1
                    );
                    assert_eq!(
                        db.query_row("SELECT count(*) FROM benchmark_drivers", [], |row| row
                            .get::<_, usize>(0))?,
                        1
                    );
                    Ok((
                        serde_json::from_slice::<BenchmarkSuiteManifest>(&suite).unwrap(),
                        serde_json::from_slice::<BenchmarkSuiteDriverStatus>(&driver).unwrap(),
                        request,
                    ))
                })
                .unwrap();
            assert_eq!(saved_suite.instance_id, destination.as_str());
            assert!(saved_suite.historical && saved_driver.historical);
            assert_eq!(saved_suite.runs[0].launch_intent, None);
            assert!(request.is_none());
            assert_eq!(saved_suite.created_at, "2026-01-01T00:00:00Z");
            assert_eq!(
                saved_suite.runs[0].launched_at.as_deref(),
                Some("2026-01-01T00:00:00Z")
            );
            assert_eq!(saved_driver.state, state);
            assert_eq!(saved_driver.error.as_deref(), Some(error));
            assert_eq!(saved_driver.last_session_id, saved_suite.runs[0].session_id);
            assert_eq!(saved_driver.suite_id, saved_suite.suite_id);
            assert_eq!(
                saved_suite.runs[0].session_id.as_deref(),
                Some(reports.list_recent(1).unwrap()[0].session_id.as_str())
            );
        }
        assert_eq!(before, crate::import::tests::snapshot(&fixture.baseline));
        write_benchmark(&fixture, "suite-drivers", DRIVER, &json!({}));
        assert!(matches!(
            inventory.prepare_instance(&preview.fingerprint, INSTANCE),
            Err(ImportError::SourceChanged)
        ));
    }
}

#[test]
fn terminal_benchmark_history_keeps_sparse_custom_plans_and_earlier_driver_snapshot() {
    let fixture = Fixture::new();
    let mut earlier = benchmark_report();
    earlier["scenario"]["benchmark_profile"] = json!("older+profile");
    let mut latest = earlier.clone();
    latest["session_id"] = json!("session-b");
    latest["launched_at"] = json!("2026-01-01T00:00:10.000Z");
    latest["recorded_at"] = json!("2026-01-01T00:00:12.000Z");
    let mut manifest = suite();
    manifest["updated_at"] = json!("2026-01-01T00:00:12Z");
    manifest["runs"][0]["run_index"] = json!(3);
    manifest["runs"][0]["profile"] = json!("older+profile");
    manifest["runs"][0]["target_id"] = json!("older+target");
    manifest["runs"][0]["session_id"] = json!("session-b");
    manifest["runs"][0]["launched_at"] = json!("2026-01-01T00:00:10Z");
    let mut stopped = driver();
    stopped["run_count"] = json!(8);
    stopped["last_run_index"] = json!(3);
    stopped["pending_run_index"] = json!(5);
    write_benchmark_history(&fixture, &earlier, &manifest, &stopped);
    write(&fixture, "session-b.json", &latest);
    let inventory = fixture.capture();
    let history = prepare_history(&inventory)
        .unwrap()
        .for_instance(INSTANCE)
        .unwrap();
    assert_eq!(history.suites[0].runs[0].run_index, 3);
    assert_eq!(history.suites[0].runs[0].profile, "older+profile");
    assert_eq!(history.drivers[0].run_count, 8);
    assert_eq!(history.drivers[0].launched_run_count, 1);
    assert_eq!(history.drivers[0].pending_run_index, Some(5));
    assert_eq!(history.drivers[0].updated_at, "2026-01-01T00:00:02Z");
    assert_ne!(
        history.drivers[0].last_session_id,
        history.suites[0].runs[0].session_id
    );
}

#[test]
fn terminal_benchmark_history_rejects_unsupported_or_incoherent_source_without_waiving_blockers() {
    for case in [
        "schema",
        "unknown_suite",
        "nonterminal_run",
        "orphan_report",
        "instance",
        "descriptor",
        "launch_time",
        "duplicate_run",
        "driver_schema",
        "nonterminal_driver",
        "active_driver",
        "handoff_state",
        "handoff_active",
        "handoff_missing_report",
        "handoff_descriptor",
        "handoff_unknown_driver",
        "limit_state",
        "limit_active",
        "limit_missing_report",
        "limit_descriptor",
        "limit_unknown_driver",
        "driver_counts",
        "detached_queued",
        "missing_last",
        "unsafe_error",
    ] {
        let fixture = Fixture::new();
        let mut manifest = suite();
        let mut status = driver();
        if case.starts_with("limit_") || case.starts_with("handoff_") {
            status["state"] = json!("interrupted");
            status["error"] = json!(if case.starts_with("handoff_") {
                "driver automatic resume queued after restart"
            } else {
                "driver ignored after restart resume limit"
            });
        }
        match case {
            "schema" => manifest["schema_version"] = json!(3),
            "unknown_suite" => manifest["future"] = json!(true),
            "nonterminal_run" => manifest["runs"][0]["state"] = json!("running"),
            "orphan_report" => manifest["runs"][0]["session_id"] = json!("missing"),
            "instance" => manifest["instance_id"] = json!(SECOND),
            "descriptor" => manifest["runs"][0]["profile"] = json!("other"),
            "launch_time" => manifest["runs"][0]["launched_at"] = json!("2026-01-01T00:00:01Z"),
            "duplicate_run" => {
                let duplicate = manifest["runs"][0].clone();
                manifest["runs"].as_array_mut().unwrap().push(duplicate);
            }
            "driver_schema" => status["schema_version"] = json!(1),
            "nonterminal_driver" => status["state"] = json!("scheduled"),
            "active_driver" => status["active_session_id"] = json!("session-a"),
            "limit_state" | "handoff_state" => status["state"] = json!("failed"),
            "limit_active" | "handoff_active" => status["active_session_id"] = json!("session-a"),
            "limit_missing_report" | "handoff_missing_report" => {
                status["last_session_id"] = json!("missing")
            }
            "limit_descriptor" | "handoff_descriptor" => {
                manifest["runs"][0]["profile"] = json!("other")
            }
            "limit_unknown_driver" | "handoff_unknown_driver" => status["future"] = json!(true),
            "driver_counts" => status["launched_run_count"] = json!(2),
            "detached_queued" => {
                status["suite_id"] = json!("suite-dev-0000000000000002");
                status["state"] = json!("interrupted");
                status["error"] = json!("driver automatic resume queued after restart");
            }
            "missing_last" => status["last_session_id"] = json!("missing"),
            "unsafe_error" => status["error"] = json!("/private/profile/secret"),
            _ => unreachable!(),
        }
        write_benchmark_history(&fixture, &benchmark_report(), &manifest, &status);
        let before = crate::import::tests::snapshot(&fixture.baseline);
        let inventory = fixture.capture();
        assert!(prepare_history(&inventory).is_err(), "{case}");
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{case}"
        );
        assert!(
            inventory
                .preview()
                .blockers
                .contains(&ImportBlocker::RetainedHistoryRequiresConversion)
        );
        assert_eq!(before, crate::import::tests::snapshot(&fixture.baseline));
    }
}

#[test]
fn terminal_benchmark_history_rejects_global_session_alias_and_oversized_records() {
    let fixture = Fixture::new();
    write_benchmark_history(&fixture, &benchmark_report(), &suite(), &driver());
    let mut duplicate = suite();
    duplicate["suite_id"] = json!("suite-dev-0000000000000002");
    write_benchmark(&fixture, "suites", "suite-dev-0000000000000002", &duplicate);
    assert!(prepare_history(&fixture.capture()).is_err());
    let fixture = Fixture::new();
    write_benchmark_history(&fixture, &benchmark_report(), &suite(), &driver());
    let mut oversized = serde_json::to_vec(&driver()).unwrap();
    oversized.resize(MAX_BENCHMARK_BYTES + 1, b' ');
    fs::write(
        fixture
            .baseline
            .join(format!("benchmarks/suite-drivers/{DRIVER}.json")),
        oversized,
    )
    .unwrap();
    assert!(matches!(
        prepare_history(&fixture.capture()),
        Err(ImportError::LimitExceeded)
    ));
}

#[test]
fn terminal_benchmark_history_accepts_both_legacy_outcome_writers_without_relabeling() {
    for (label, kind, reason, states) in [
        ("failed", "unknown", "unknown_exit", ["failed", "exited"]),
        ("exited", "failed", "startup_stalled", ["exited", "failed"]),
        ("completed", "clean", "clean_exit", ["completed", "exited"]),
    ] {
        for state in states {
            let fixture = Fixture::new();
            let mut proof = benchmark_report();
            proof["outcome"] = json!(label);
            let summary = match kind {
                "clean" => "Minecraft exited cleanly.",
                "failed" => "Minecraft did not finish startup in time.",
                _ => "Minecraft exited and the launcher could not classify the reason.",
            };
            proof["session_outcome"] = json!({"kind":kind,"reason":reason,"summary":summary});
            let mut manifest = suite();
            manifest["runs"][0]["state"] = json!(state);
            write_benchmark_history(&fixture, &proof, &manifest, &driver());
            let history = prepare_history(&fixture.capture())
                .unwrap()
                .for_instance(INSTANCE)
                .unwrap();
            assert_eq!(history.suites[0].runs[0].state, state);
        }
    }
}

#[test]
fn comparison_uses_legacy_unknown_dimension_semantics_and_rejects_false_evidence() {
    let mut value = report();
    value["scenario"]["benchmark_profile"] = json!("unknown");
    value["scenario"]["benchmark_run_type"] = json!("unknown");
    value["comparison"] = json!({"baseline_session_id":"session-before","baseline_recorded_at":"2025-12-31T00:00:00.000Z",
        "baseline":{"performance_mode":"vanilla","version_id":"1.21.1","requested_memory_mb":1024,"device_tier":"mid"},
        "matched_sample_count":3,"metric_name":"boot_duration_ms","current_value_ms":1000,"baseline_value_ms":2000,"delta_ms":-1000,"delta_percent":-50.0});
    let retained = converted(value.clone()).unwrap();
    assert_eq!(
        retained.scenario.benchmark_profile.as_deref(),
        Some("unknown")
    );
    assert!(
        retained
            .comparison
            .unwrap()
            .baseline
            .benchmark_profile
            .is_none()
    );
    for (field, incorrect) in [
        ("delta_ms", json!(0)),
        ("delta_percent", json!(50.0)),
        ("current_value_ms", json!(2000)),
        ("baseline_value_ms", json!(0)),
        ("baseline_session_id", json!("session-a")),
        ("baseline_recorded_at", json!("2027-01-01T00:00:00.000Z")),
    ] {
        let mut incorrect_report = value.clone();
        incorrect_report["comparison"][field] = incorrect;
        assert!(converted(incorrect_report).is_err(), "accepted {field}");
    }
}
