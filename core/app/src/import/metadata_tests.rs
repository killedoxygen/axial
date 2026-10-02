use super::*;
use crate::{
    import::{
        ImportPreviews,
        tests::{Fixture, snapshot},
    },
    library::{LibraryLifecycle, LibraryOpenOutcome},
    settings::{ConfigPatch, ConfigTheme, STATE_INSPECTOR_FLAG},
    storage::{MetadataStore, StorageError},
};
use serde_json::{Value, json};
use std::{fs, path::Path};

const FIRST: &str = "offline-92d74dda76fe332cb669d0daddfb7952";
const SECOND: &str = "offline-761ba00914be3766a1c04a87a961857c";

struct Destination {
    accounts: AccountDirectory,
    settings: SettingsStore,
    library: LibraryLifecycle,
    _root: tempfile::TempDir,
}

impl Destination {
    fn new() -> Self {
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated destination root: {other:?}"),
        };
        let (settings, accounts) = stores(&root.path().join("metadata.sqlite"));
        Self {
            accounts,
            settings,
            library,
            _root: root,
        }
    }

    fn commit(
        &self,
        prepared: &PreparedMetadataImport,
        request: &MetadataImportRequest,
    ) -> Result<MetadataImportCommit, MetadataImportError> {
        prepared.commit(
            &self.settings,
            &self.accounts,
            &self.library.admit_application_root().unwrap(),
            request,
            &CancellationToken::new(),
        )
    }
}

fn stores(path: &Path) -> (SettingsStore, AccountDirectory) {
    let store = Arc::new(MetadataStore::open(path).unwrap());
    store
        .migrate(&[
            METADATA_IMPORT_MIGRATION,
            METADATA_IMPORT_IDENTITIES_MIGRATION,
            METADATA_IMPORT_HISTORY_MIGRATION,
            METADATA_IMPORT_ARCHIVED_REPORTS_MIGRATION,
            METADATA_IMPORT_ARCHIVED_BENCHMARKS_MIGRATION,
            METADATA_IMPORT_ARCHIVED_OPERATIONS_MIGRATION,
            install_history::MIGRATION,
            crate::launch::reports::REPORT_MIGRATION,
            crate::performance::benchmarks::MIGRATION,
            crate::performance::benchmarks::MIGRATION_V2,
            crate::performance::benchmarks::MIGRATION_V3,
            crate::performance::mutation::MIGRATION,
            crate::performance::mutation::MIGRATION_V2,
        ])
        .unwrap();
    let settings = SettingsStore::new_with_telemetry_identity(Arc::clone(&store), true).unwrap();
    let accounts = AccountDirectory::new(store).unwrap();
    (settings, accounts)
}

fn prepare(source: &Fixture) -> (PreparedMetadataImport, MetadataImportRequest) {
    let previews = ImportPreviews::new();
    let preview = previews.admit(source.capture()).unwrap();
    assert!(preview.metadata_import_available);
    let prepared = previews.prepare_metadata(&preview.fingerprint).unwrap();
    let request = MetadataImportRequest {
        metadata_import_id: preview.metadata_import_id,
        fingerprint: preview.fingerprint,
        expected_settings_revision: 0,
        expected_account_selection_revision: 0,
    };
    // Accepted work retains its capability independently of preview lifetime.
    previews.forget().unwrap();
    (prepared, request)
}

fn config(source: &Fixture, change: impl FnOnce(&mut Value)) {
    let path = source.baseline.join("config.json");
    let mut value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    change(&mut value);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn global_journal(source: &Fixture, change: impl FnOnce(&mut Value)) {
    let mut journal = crate::import::tests::successful_install_journal();
    journal["entries"].as_array_mut().unwrap().truncate(2);
    change(&mut journal);
    let path = source.baseline.join("state/operation-journals.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
}

fn archived_performance_journal(source: &Fixture, change: impl FnOnce(&mut Value)) {
    let mut journal = crate::import::tests::terminal_performance_journal();
    for entry in journal["entries"].as_array_mut().unwrap() {
        entry["intent"]["intent"]["instance_id"] = json!("0000000000000002");
        entry["targets"][0]["id"] = json!("0000000000000002");
    }
    change(&mut journal);
    let path = source.baseline.join("state/operation-journals.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
}

#[test]
fn archived_performance_metadata_preserves_missing_target_evidence_and_reopen() {
    let source = Fixture::new();
    archived_performance_journal(&source, |_| {});
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let receipt = destination
        .commit(&prepared, &request)
        .unwrap()
        .response
        .receipt;
    assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
    assert_eq!(destination.settings.current().unwrap().revision, 1);
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["archived_performance_operation_count"],
        6
    );
    let records = performance_rows(&destination);
    assert_eq!(records.len(), 6);
    let journal: Value = serde_json::from_slice(
        &fs::read(source.baseline.join("state/operation-journals.json")).unwrap(),
    )
    .unwrap();
    for (_, instance, state, bytes) in &records {
        assert_eq!(
            instance,
            &format!("archived-{}-0000000000000002", prepared.source_id)
        );
        assert_eq!(state, "historical");
        let value: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(value["instance_id"], *instance);
        assert_eq!(
            value["evidence"]["intent"]["instance_id"],
            "0000000000000002"
        );
        let original = journal["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["operation_id"] == value["evidence"]["operation_id"])
            .unwrap();
        assert_eq!(value["evidence"]["intent"], original["intent"]["intent"]);
        assert_eq!(
            value["evidence"]["terminal"],
            original["intent"]["phase"]["terminal"]
        );
        assert_eq!(value["created_at"], original["intent"]["created_at"]);
        assert_eq!(value["updated_at"], original["intent"]["updated_at"]);
    }
    assert_eq!(
        destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt,
        receipt
    );
    let (reopened, _) = stores(&destination._root.path().join("metadata.sqlite"));
    assert_eq!(
        metadata_status(&reopened, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(receipt)
    );
    assert_eq!(performance_rows(&destination), records);
    let pending = destination
        .settings
        .metadata()
        .read(|db| -> Result<u32, StorageError> {
            Ok(
                db.query_row("SELECT count(*) FROM performance_operations", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(pending, 0);
    assert_eq!(snapshot(&source.baseline), before);
}

fn performance_rows(destination: &Destination) -> Vec<(String, String, String, Vec<u8>)> {
    destination
        .settings
        .metadata()
        .read(|db| -> Result<_, StorageError> {
            Ok(db
                .prepare(
                    "SELECT id,instance_id,state,payload FROM performance_commands ORDER BY id",
                )?
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<_, _>>()?)
        })
        .unwrap()
}

#[test]
fn archived_performance_old_receipt_completion_preserves_later_edits() {
    let source = Fixture::new();
    archived_performance_journal(&source, |_| {});
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let mut old = prepared.clone();
    old.archived_operations = None;
    let mut receipt = destination.commit(&old, &request).unwrap().response.receipt;
    assert!(
        !serde_json::to_value(&receipt)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("archived_performance_operation_count")
    );
    assert!(performance_rows(&destination).is_empty());
    destination.accounts.select(SECOND).unwrap();
    let accounts = destination.accounts.snapshot().unwrap();
    let config = destination
        .settings
        .update(ConfigPatch {
            expected_revision: 1,
            theme: Some(ConfigTheme::Birch),
            ..ConfigPatch::default()
        })
        .unwrap();
    let changes = destination.settings.subscribe().unwrap();
    receipt.archived_performance_operation_count = Some(6);
    let completed = destination.commit(&prepared, &request).unwrap();
    assert!(completed.response.already_imported);
    assert_eq!(completed.response.receipt, receipt);
    assert_eq!(completed.settings.config, config);
    assert_eq!(destination.accounts.snapshot().unwrap(), accounts);
    assert!(!changes.has_changed().unwrap());
    let rows = performance_rows(&destination);
    assert_eq!(rows.len(), 6);
    destination.commit(&prepared, &request).unwrap();
    let (reopened, _) = stores(&destination._root.path().join("metadata.sqlite"));
    assert_eq!(
        metadata_status(&reopened, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(receipt)
    );
    assert_eq!(performance_rows(&destination), rows);
}

#[test]
fn archived_performance_optional_refusal_never_claims_missing_or_unsupported_history_empty() {
    for case in [
        "empty",
        "live",
        "schema",
        "duplicate",
        "active",
        "spoof",
        "bad_proof",
        "registry",
        "malformed",
        "directory",
        "ancestor_file",
        "state_alias",
        "file_alias",
    ] {
        let source = Fixture::new();
        archived_report(&source, |_| {});
        if case != "empty" {
            archived_performance_journal(&source, |journal| match case {
                "live" => {
                    for entry in journal["entries"].as_array_mut().unwrap() {
                        entry["intent"]["intent"]["instance_id"] = json!("0000000000000001");
                        entry["targets"][0]["id"] = json!("0000000000000001");
                    }
                }
                "schema" => journal["schema"] = json!("axial.state.operation_journals.v9"),
                "duplicate" => journal["entries"][1] = journal["entries"][0].clone(),
                "active" => journal["entries"][0]["intent"]["phase"]["phase"] = json!("prepared"),
                "spoof" => journal["entries"][0]["intent"] = json!({"kind":"generic"}),
                "bad_proof" => {
                    journal["entries"][5]["intent"]["phase"]["terminal"]["prepared"]["proof"]["graph_sha512"] =
                        json!("invalid")
                }
                _ => {}
            });
        }
        let path = source.baseline.join("state/operation-journals.json");
        match case {
            "registry" => {
                let path = source.baseline.join("instances.json");
                let mut registry: Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                registry["schema_version"] = json!(4);
                fs::write(path, serde_json::to_vec(&registry).unwrap()).unwrap();
            }
            "malformed" => fs::write(path, b"{").unwrap(),
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "ancestor_file" | "state_alias" | "file_alias" => {
                let bytes = fs::read(&path).unwrap();
                fs::remove_file(&path).unwrap();
                if case == "file_alias" {
                    fs::write(source.baseline.join("state/Operation-journals.json"), bytes)
                        .unwrap();
                } else {
                    fs::remove_dir(source.baseline.join("state")).unwrap();
                    if case == "ancestor_file" {
                        fs::write(source.baseline.join("state"), bytes).unwrap();
                    } else {
                        fs::create_dir(source.baseline.join("State")).unwrap();
                        fs::write(source.baseline.join("State/operation-journals.json"), bytes)
                            .unwrap();
                    }
                }
            }
            _ => {}
        }
        let before = snapshot(&source.baseline);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let receipt = destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt;
        assert_eq!(
            receipt.archived_performance_operation_count,
            matches!(case, "empty" | "live").then_some(0),
            "{case}"
        );
        assert_eq!(
            receipt.archived_launch_report_count,
            (case != "registry").then_some(1),
            "{case}"
        );
        if matches!(
            case,
            "directory" | "ancestor_file" | "state_alias" | "file_alias"
        ) {
            assert_eq!(receipt.global_install_history_count, None);
            assert!(crate::import::history::prepare_rules_history(&source.capture()).is_err());
        }
        assert!(performance_rows(&destination).is_empty());
        assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn archived_performance_publication_and_late_settings_write_are_atomic() {
    for old in [false, true] {
        for effect in [
            "ignore_record",
            "ignore_receipt",
            "rewrite_record",
            "late_settings",
        ] {
            if old && effect == "late_settings" {
                continue;
            }
            let source = Fixture::new();
            archived_performance_journal(&source, |_| {});
            let (prepared, request) = prepare(&source);
            let destination = Destination::new();
            if old {
                let mut previous = prepared.clone();
                previous.archived_operations = None;
                destination.commit(&previous, &request).unwrap();
            }
            let config = destination.settings.current().unwrap();
            let accounts = destination.accounts.snapshot().unwrap();
            let receipt = metadata_status(&destination.settings, &request.metadata_import_id)
                .unwrap()
                .receipt;
            let changes = destination.settings.subscribe().unwrap();
            let action = if old {
                "UPDATE OF archived_performance_operation_proof"
            } else {
                "INSERT"
            };
            let trigger = match effect {
                "ignore_record" => "BEFORE INSERT ON performance_commands BEGIN SELECT RAISE(IGNORE); END;".to_owned(),
                "ignore_receipt" => format!("BEFORE {action} ON profile_metadata_imports BEGIN SELECT RAISE(IGNORE); END;"),
                "rewrite_record" => format!("AFTER {action} ON profile_metadata_imports BEGIN UPDATE performance_commands SET payload=CAST('{{}}' AS BLOB); END;"),
                _ => "AFTER UPDATE ON settings_config BEGIN UPDATE performance_commands SET state='queued'; END;".to_owned(),
            };
            destination
                .settings
                .metadata()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(&format!("CREATE TRIGGER corrupt_operations {trigger}"))?;
                    Ok(())
                })
                .unwrap();
            assert!(
                destination.commit(&prepared, &request).is_err(),
                "{old}/{effect}"
            );
            assert_eq!(destination.settings.current().unwrap(), config);
            assert_eq!(destination.accounts.snapshot().unwrap(), accounts);
            assert_eq!(
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt,
                receipt
            );
            assert!(performance_rows(&destination).is_empty());
            assert!(!changes.has_changed().unwrap());
        }
    }
}

#[cfg(unix)]
#[test]
fn archived_performance_unsafe_journal_absence_preserves_independent_metadata() {
    for ancestor in [false, true] {
        let source = Fixture::new();
        archived_report(&source, |_| {});
        let external = source.baseline.parent().unwrap().join("private-journal");
        fs::write(&external, b"not admitted").unwrap();
        let link = if ancestor {
            source.baseline.join("state")
        } else {
            fs::create_dir(source.baseline.join("state")).unwrap();
            source.baseline.join("state/operation-journals.json")
        };
        std::os::unix::fs::symlink(&external, &link).unwrap();
        let before = snapshot(&source.baseline);
        let inventory = source.capture();
        assert!(crate::import::history::prepare_history(&inventory).is_err());
        assert!(crate::import::history::prepare_rules_history(&inventory).is_err());
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let receipt = destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt;
        assert_eq!(receipt.archived_performance_operation_count, None);
        assert_eq!(receipt.global_install_history_count, None);
        assert_eq!(receipt.archived_launch_report_count, Some(1));
        assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
        assert_eq!(fs::read(external).unwrap(), b"not admitted");
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn archived_performance_completed_receipt_rejects_corruption_and_oversized_proofs_without_repair() {
    for corruption in [
        "missing",
        "payload",
        "binding",
        "state",
        "proof",
        "oversized",
        "multibyte",
    ] {
        let source = Fixture::new();
        archived_performance_journal(&source, |_| {});
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        destination.commit(&prepared, &request).unwrap();
        destination.settings.metadata().transaction(|tx| -> Result<(), StorageError> {
            match corruption {
                "missing" => { tx.execute("DELETE FROM performance_commands", [])?; }
                "payload" => { tx.execute("UPDATE performance_commands SET payload=CAST('{}' AS BLOB)", [])?; }
                "binding" => { tx.execute("UPDATE performance_commands SET instance_id='00000000-0000-4000-8000-000000000001'", [])?; }
                "state" => { tx.execute("UPDATE performance_commands SET state='queued'", [])?; }
                _ => {
                    let proof = match corruption { "oversized" => " ".repeat(16*1024+1), "multibyte" => "é".repeat(9*1024), _ => "{}".to_owned() };
                    if matches!(corruption, "oversized" | "multibyte") {
                        assert!(tx.execute("UPDATE profile_metadata_imports SET archived_performance_operation_proof=?1", [&proof]).is_err());
                        tx.execute_batch("PRAGMA ignore_check_constraints=ON")?;
                    }
                    tx.execute("UPDATE profile_metadata_imports SET archived_performance_operation_proof=?1", [&proof])?;
                    tx.execute_batch("PRAGMA ignore_check_constraints=OFF")?;
                }
            }
            Ok(())
        }).unwrap();
        let rows = performance_rows(&destination);
        assert!(
            metadata_status(&destination.settings, &request.metadata_import_id).is_err(),
            "{corruption}"
        );
        assert!(
            destination.commit(&prepared, &request).is_err(),
            "{corruption}"
        );
        assert_eq!(performance_rows(&destination), rows);
        assert_eq!(destination.settings.current().unwrap().revision, 1);
    }
}

fn archived_report(source: &Fixture, change: impl FnOnce(&mut Value)) {
    let mut report = json!({
        "schema":"axial.launch.proof","schema_version":3,"session_id":"session-archived",
        "instance_id":"0000000000000002","version_id":"1.21.1",
        "launched_at":"2026-01-01T00:00:00.000Z","recorded_at":"2026-01-01T00:00:02.000Z",
        "outcome":"exited","session_outcome":{"reason":"clean_exit","kind":"clean","summary":"Minecraft exited cleanly."},
        "scenario":{"scenario_id":"vanilla_launch","performance_mode":"vanilla","requested_memory_mb":1024,"version_id":"1.21.1"},
        "device":{"tier":"mid","total_memory_mb":8192,"cpu_threads":8},"exit_code":0,"stages":[]
    });
    change(&mut report);
    let path = source
        .baseline
        .join("benchmarks/launch/session-archived.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&report).unwrap()).unwrap();
}

fn archived_rows(destination: &Destination) -> Vec<(String, String, Vec<u8>)> {
    destination
        .settings
        .metadata()
        .read(|db| -> Result<_, StorageError> {
            Ok(db
                .prepare(
                    "SELECT session_id,instance_id,payload FROM launch_reports ORDER BY session_id",
                )?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<_, _>>()?)
        })
        .unwrap()
}

fn archived_benchmark(source: &Fixture) {
    archived_report(source, |report| {
        report["scenario"]["benchmark_profile"] = json!("vanilla_baseline");
        report["scenario"]["benchmark_run_type"] = json!("coldish");
        report["scenario"]["benchmark_mode"] = json!("development");
        report["scenario"]["benchmark_id"] = json!("benchmark-0000000000000001");
    });
    for (path, value) in [
        (
            "suites/suite-dev-0000000000000002.json",
            json!({
                "schema":"axial.launch.benchmark.suite","schema_version":2,
                "suite_id":"suite-dev-0000000000000002","instance_id":"0000000000000002","mode":"development",
                "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z",
                "runs":[{"run_index":0,"profile":"vanilla_baseline","run_type":"coldish","target_id":"",
                    "benchmark_id":"benchmark-0000000000000001","session_id":"session-archived",
                    "launched_at":"2026-01-01T00:00:00Z","state":"exited"},
                    {"run_index":1,"profile":"vanilla_baseline","run_type":"warm","target_id":"",
                    "benchmark_id":"benchmark-0000000000000002","session_id":null,"launched_at":null,"state":"pending"}]
            }),
        ),
        (
            "suite-drivers/benchmark-suite-driver-0000000000000002.json",
            json!({
                "id":"benchmark-suite-driver-0000000000000002","suite_id":"suite-dev-0000000000000002",
                "mode":"development","state":"stopped","interval_ms":30000,"run_count":2,"launched_run_count":1,
                "pending_run_index":1,"active_session_id":null,"last_run_index":0,"last_session_id":"session-archived",
                "error":"Stopped by the user","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:02Z"
            }),
        ),
    ] {
        let path = source.baseline.join("benchmarks").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

#[test]
fn archived_benchmarks_metadata_preserves_report_backed_plan_and_inactive_driver() {
    let source = Fixture::new();
    archived_benchmark(&source);
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let receipt = destination
        .commit(&prepared, &request)
        .unwrap()
        .response
        .receipt;
    assert_eq!(receipt.archived_launch_report_count, Some(1));
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["archived_benchmark_count"],
        2
    );
    let counts = destination.settings.metadata().read(|db| -> Result<_, StorageError> {
        Ok(db.query_row("SELECT (SELECT count(*) FROM benchmark_suites), (SELECT count(*) FROM benchmark_drivers), (SELECT count(*) FROM benchmark_drivers WHERE request IS NOT NULL OR source_driver_id IS NOT NULL)", [], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?, row.get::<_, u32>(2)?)))?)
    }).unwrap();
    assert_eq!(counts, (1, 1, 0));
    assert_eq!(
        destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt,
        receipt
    );
    let (reopened, _) = stores(&destination._root.path().join("metadata.sqlite"));
    assert_eq!(
        metadata_status(&reopened, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(receipt)
    );
    assert_eq!(snapshot(&source.baseline), before);
}

#[test]
fn detached_driver_metadata_preserves_pruned_parent_reference_and_reopen() {
    let source = Fixture::new();
    archived_benchmark(&source);
    fs::remove_file(
        source
            .baseline
            .join("benchmarks/suites/suite-dev-0000000000000002.json"),
    )
    .unwrap();
    fs::remove_file(
        source
            .baseline
            .join("benchmarks/launch/session-archived.json"),
    )
    .unwrap();
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let receipt = destination
        .commit(&prepared, &request)
        .unwrap()
        .response
        .receipt;
    assert_eq!(receipt.archived_launch_report_count, Some(0));
    assert_eq!(receipt.archived_benchmark_count, Some(1));
    let rows = benchmark_rows(&destination);
    assert_eq!(rows.len(), 1);
    let driver: Value = serde_json::from_slice(&rows[0].1).unwrap();
    assert_eq!(driver["historical"], true);
    assert!(
        driver["suite_id"]
            .as_str()
            .unwrap()
            .starts_with("legacy-suite-")
    );
    assert!(
        driver["last_session_id"]
            .as_str()
            .unwrap()
            .starts_with("legacy-")
    );
    assert_eq!(driver["last_run_index"], 0);
    assert_eq!(driver["pending_run_index"], 1);
    assert!(rows[0].2.is_none() && rows[0].3.is_none());
    let counts = destination.settings.metadata().read(|db| -> Result<_, StorageError> {
        Ok(db.query_row("SELECT (SELECT count(*) FROM benchmark_suites),(SELECT count(*) FROM launch_reports)", [], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)))?)
    }).unwrap();
    assert_eq!(counts, (0, 0));
    assert_eq!(
        destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt,
        receipt
    );
    let (reopened, _) = stores(&destination._root.path().join("metadata.sqlite"));
    assert_eq!(
        metadata_status(&reopened, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(receipt)
    );
    assert_eq!(benchmark_rows(&destination), rows);
    assert_eq!(snapshot(&source.baseline), before);
    destination
        .settings
        .metadata()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute("UPDATE benchmark_drivers SET detached_source=NULL", [])?;
            Ok(())
        })
        .unwrap();
    assert!(metadata_status(&reopened, &request.metadata_import_id).is_err());
    assert!(destination.commit(&prepared, &request).is_err());
    let missing_provenance: Option<String> = destination
        .settings
        .metadata()
        .read(|db| -> Result<_, StorageError> {
            Ok(
                db.query_row("SELECT detached_source FROM benchmark_drivers", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert!(missing_provenance.is_none());
    assert_eq!(benchmark_rows(&destination), rows);
}

fn benchmark_rows(
    destination: &Destination,
) -> Vec<(String, Vec<u8>, Option<Vec<u8>>, Option<String>)> {
    destination.settings.metadata().read(|db| -> Result<_, StorageError> {
        Ok(db.prepare("SELECT suite_id,payload,NULL,source_suite_id FROM benchmark_suites UNION ALL SELECT driver_id,payload,request,source_driver_id FROM benchmark_drivers ORDER BY 1")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<Result<_, _>>()?)
    }).unwrap()
}

#[test]
fn archived_benchmarks_old_receipt_completion_preserves_later_edits() {
    for detached in [false, true] {
        let source = Fixture::new();
        archived_benchmark(&source);
        if detached {
            fs::remove_file(
                source
                    .baseline
                    .join("benchmarks/suites/suite-dev-0000000000000002.json"),
            )
            .unwrap();
        }
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let mut old = prepared.clone();
        old.archived_benchmarks = None;
        let mut expected = destination.commit(&old, &request).unwrap().response.receipt;
        assert!(
            !serde_json::to_value(&expected)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("archived_benchmark_count")
        );
        let reports = archived_rows(&destination);
        destination.accounts.select(SECOND).unwrap();
        let accounts = destination.accounts.snapshot().unwrap();
        let config = destination
            .settings
            .update(ConfigPatch {
                expected_revision: 1,
                theme: Some(ConfigTheme::Birch),
                ..ConfigPatch::default()
            })
            .unwrap();
        let changes = destination.settings.subscribe().unwrap();
        expected.archived_benchmark_count = Some(if detached { 1 } else { 2 });
        let completed = destination.commit(&prepared, &request).unwrap();
        assert!(completed.response.already_imported);
        assert_eq!(completed.response.receipt, expected);
        assert_eq!(completed.settings.config, config);
        assert_eq!(destination.accounts.snapshot().unwrap(), accounts);
        assert_eq!(archived_rows(&destination), reports);
        assert!(!changes.has_changed().unwrap());
        let rows = benchmark_rows(&destination);
        assert_eq!(rows.len(), if detached { 1 } else { 2 });
        assert!(
            rows.iter()
                .all(|(_, _, request, parent)| request.is_none() && parent.is_none())
        );
        let (reopened, _) = stores(&destination._root.path().join("metadata.sqlite"));
        assert_eq!(
            metadata_status(&reopened, &request.metadata_import_id)
                .unwrap()
                .receipt,
            Some(expected)
        );
        destination.commit(&prepared, &request).unwrap();
        assert_eq!(benchmark_rows(&destination), rows);
    }
}

#[test]
fn archived_benchmarks_optional_refusal_preserves_reports_and_metadata() {
    for variant in [
        "empty",
        "active_driver",
        "active_session",
        "running_run",
        "partial_pair",
        "detached_queued",
        "missing_report",
        "wrong_report",
        "unsupported_registry",
    ] {
        let source = Fixture::new();
        if variant != "empty" {
            archived_benchmark(&source);
        }
        let (relative, field, value) = match variant {
            "active_driver" => (
                "benchmarks/suite-drivers/benchmark-suite-driver-0000000000000002.json",
                "state",
                json!("active"),
            ),
            "active_session" => (
                "benchmarks/suite-drivers/benchmark-suite-driver-0000000000000002.json",
                "active_session_id",
                json!("session-archived"),
            ),
            "unsupported_registry" => ("instances.json", "schema_version", json!(4)),
            _ => ("", "", Value::Null),
        };
        if !relative.is_empty() {
            let path = source.baseline.join(relative);
            let mut value_before: Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value_before[field] = value;
            fs::write(path, serde_json::to_vec(&value_before).unwrap()).unwrap();
        }
        match variant {
            "detached_queued" => {
                fs::remove_file(
                    source
                        .baseline
                        .join("benchmarks/suites/suite-dev-0000000000000002.json"),
                )
                .unwrap();
                let path = source
                    .baseline
                    .join("benchmarks/suite-drivers/benchmark-suite-driver-0000000000000002.json");
                let mut driver: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                driver["state"] = json!("interrupted");
                driver["error"] = json!("driver automatic resume queued after restart");
                fs::write(path, serde_json::to_vec(&driver).unwrap()).unwrap();
            }
            "missing_report" => fs::remove_file(
                source
                    .baseline
                    .join("benchmarks/launch/session-archived.json"),
            )
            .unwrap(),
            "wrong_report" => archived_report(&source, |_| {}),
            "running_run" | "partial_pair" => {
                let path = source
                    .baseline
                    .join("benchmarks/suites/suite-dev-0000000000000002.json");
                let mut suite: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                if variant == "running_run" {
                    suite["runs"][0]["state"] = json!("running");
                } else {
                    suite["runs"][1]["session_id"] = json!("session-archived");
                }
                fs::write(path, serde_json::to_vec(&suite).unwrap()).unwrap();
            }
            _ => {}
        }
        let before = snapshot(&source.baseline);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let receipt = destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt;
        assert_eq!(
            receipt.archived_benchmark_count,
            (variant == "empty").then_some(0),
            "{variant}"
        );
        let report_count = match variant {
            "empty" | "missing_report" => Some(0),
            "unsupported_registry" => None,
            _ => Some(1),
        };
        assert_eq!(
            receipt.archived_launch_report_count, report_count,
            "{variant}"
        );
        assert_eq!(
            archived_rows(&destination).len(),
            report_count.unwrap_or_default()
        );
        assert!(benchmark_rows(&destination).is_empty());
        assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn detached_driver_unobservable_namespaces_do_not_claim_empty_metadata_completion() {
    #[cfg(not(unix))]
    let shapes = ["file", "nested"];
    #[cfg(unix)]
    let shapes = [
        "file",
        "nested",
        "symlink_namespace",
        "symlink_record",
        "hardlink_record",
    ];
    for namespace in ["suites", "suite-drivers"] {
        for shape in shapes {
            let source = Fixture::new();
            archived_report(&source, |_| {});
            let path = source.baseline.join("benchmarks").join(namespace);
            match shape {
                "file" => fs::write(&path, b"unobserved history").unwrap(),
                "nested" => fs::create_dir_all(path.join("retained.json")).unwrap(),
                #[cfg(unix)]
                _ => {
                    let external = source
                        .baseline
                        .parent()
                        .unwrap()
                        .join("private-history.json");
                    fs::write(&external, b"unobserved history").unwrap();
                    if shape == "symlink_namespace" {
                        std::os::unix::fs::symlink(&external, &path).unwrap();
                    } else {
                        fs::create_dir_all(&path).unwrap();
                        if shape == "symlink_record" {
                            std::os::unix::fs::symlink(&external, path.join("retained.json"))
                                .unwrap();
                        } else {
                            fs::hard_link(&external, path.join("retained.json")).unwrap();
                        }
                    }
                }
                #[cfg(not(unix))]
                _ => unreachable!(),
            }
            let before = snapshot(&source.baseline);
            if shape == "hardlink_record" {
                assert!(matches!(
                    source.try_capture(),
                    Err(crate::import::ImportError::Io(_))
                ));
                assert_eq!(
                    fs::read(
                        source
                            .baseline
                            .parent()
                            .unwrap()
                            .join("private-history.json")
                    )
                    .unwrap(),
                    b"unobserved history"
                );
                assert_eq!(snapshot(&source.baseline), before);
                continue;
            }
            let inventory = source.capture();
            assert!(!inventory.file_manifests().any(|file| {
                file.relative.starts_with("profile/benchmarks/suites/")
                    || file
                        .relative
                        .starts_with("profile/benchmarks/suite-drivers/")
            }));
            assert!(crate::import::history::prepare_history(&inventory).is_err());
            let (prepared, request) = prepare(&source);
            let destination = Destination::new();
            let receipt = destination
                .commit(&prepared, &request)
                .unwrap()
                .response
                .receipt;
            assert_eq!(
                receipt.archived_benchmark_count, None,
                "{namespace}/{shape}"
            );
            assert_eq!(receipt.archived_launch_report_count, Some(1));
            assert_eq!(archived_rows(&destination).len(), 1);
            assert!(benchmark_rows(&destination).is_empty());
            assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
            assert_eq!(snapshot(&source.baseline), before);
        }
    }
}

#[test]
fn archived_benchmarks_publication_and_late_writes_are_atomic() {
    for old in [false, true] {
        for effect in [
            "ignore_suite",
            "ignore_driver",
            "ignore_receipt",
            "rewrite_suite",
            "rewrite_driver",
            "late_settings",
        ] {
            if old && effect == "late_settings" {
                continue;
            }
            let source = Fixture::new();
            archived_benchmark(&source);
            let (prepared, request) = prepare(&source);
            let destination = Destination::new();
            if old {
                let mut previous = prepared.clone();
                previous.archived_benchmarks = None;
                destination.commit(&previous, &request).unwrap();
            }
            let before_config = destination.settings.current().unwrap();
            let before_accounts = destination.accounts.snapshot().unwrap();
            let before_receipt =
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt;
            let before_reports = archived_rows(&destination);
            let changes = destination.settings.subscribe().unwrap();
            let action = if old {
                "UPDATE OF archived_benchmark_proof"
            } else {
                "INSERT"
            };
            let trigger = match effect {
                "ignore_suite" => "BEFORE INSERT ON benchmark_suites BEGIN SELECT RAISE(IGNORE); END;".to_owned(),
                "ignore_driver" => "BEFORE INSERT ON benchmark_drivers BEGIN SELECT RAISE(IGNORE); END;".to_owned(),
                "ignore_receipt" => format!("BEFORE {action} ON profile_metadata_imports BEGIN SELECT RAISE(IGNORE); END;"),
                "rewrite_suite" => format!("AFTER {action} ON profile_metadata_imports BEGIN UPDATE benchmark_suites SET payload=CAST('{{}}' AS BLOB); END;"),
                "rewrite_driver" => format!("AFTER {action} ON profile_metadata_imports BEGIN UPDATE benchmark_drivers SET payload=CAST('{{}}' AS BLOB); END;"),
                _ => "AFTER UPDATE ON settings_config BEGIN UPDATE benchmark_drivers SET request=CAST('{}' AS BLOB); END;".to_owned(),
            };
            destination
                .settings
                .metadata()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(&format!("CREATE TRIGGER corrupt_benchmarks {trigger}"))?;
                    Ok(())
                })
                .unwrap();
            assert!(
                destination.commit(&prepared, &request).is_err(),
                "{old}/{effect}"
            );
            assert_eq!(destination.settings.current().unwrap(), before_config);
            assert_eq!(destination.accounts.snapshot().unwrap(), before_accounts);
            assert_eq!(
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt,
                before_receipt
            );
            assert_eq!(archived_rows(&destination), before_reports);
            assert!(benchmark_rows(&destination).is_empty());
            assert!(!changes.has_changed().unwrap());
        }
    }
}

#[test]
fn archived_benchmarks_completed_proof_is_verify_only() {
    for corruption in [
        "missing_suite",
        "missing_driver",
        "payload",
        "request",
        "link",
        "proof",
        "missing_report",
    ] {
        let source = Fixture::new();
        archived_benchmark(&source);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        destination.commit(&prepared, &request).unwrap();
        destination
            .settings
            .metadata()
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch(match corruption {
                    "missing_suite" => "DELETE FROM benchmark_suites;",
                    "missing_driver" => "DELETE FROM benchmark_drivers;",
                    "payload" => "UPDATE benchmark_drivers SET payload=CAST('{}' AS BLOB);",
                    "request" => "UPDATE benchmark_drivers SET request=CAST('{}' AS BLOB);",
                    "link" => "UPDATE benchmark_suites SET source_suite_id='another-suite';",
                    "proof" => "UPDATE profile_metadata_imports SET archived_benchmark_proof='{}';",
                    _ => "DELETE FROM launch_reports;",
                })?;
                Ok(())
            })
            .unwrap();
        let before = benchmark_rows(&destination);
        let reports = archived_rows(&destination);
        assert!(
            metadata_status(&destination.settings, &request.metadata_import_id).is_err(),
            "{corruption}"
        );
        assert!(
            destination.commit(&prepared, &request).is_err(),
            "{corruption}"
        );
        assert_eq!(benchmark_rows(&destination), before);
        assert_eq!(archived_rows(&destination), reports);
        assert_eq!(destination.settings.current().unwrap().revision, 1);
    }
}

#[test]
fn archived_reports_old_receipt_completion_preserves_edits_and_reopen() {
    let source = Fixture::new();
    archived_report(&source, |_| {});
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let mut metadata_only = prepared.clone();
    metadata_only.archived_reports = None;
    metadata_only.archived_benchmarks = None;
    let original = destination
        .commit(&metadata_only, &request)
        .unwrap()
        .response
        .receipt;
    assert_eq!(original.archived_launch_report_count, None);
    assert!(
        !serde_json::to_value(&original)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("archived_launch_report_count")
    );
    destination.accounts.select(SECOND).unwrap();
    let accounts = destination.accounts.snapshot().unwrap();
    let config = destination
        .settings
        .update(ConfigPatch {
            expected_revision: 1,
            theme: Some(ConfigTheme::Birch),
            ..ConfigPatch::default()
        })
        .unwrap();
    let changes = destination.settings.subscribe().unwrap();
    let completed = destination.commit(&prepared, &request).unwrap();
    let mut expected = original;
    expected.archived_launch_report_count = Some(1);
    expected.archived_benchmark_count = Some(0);
    assert!(completed.response.already_imported);
    assert_eq!(completed.response.receipt, expected);
    assert_eq!(completed.settings.config, config);
    assert_eq!(destination.accounts.snapshot().unwrap(), accounts);
    assert!(!changes.has_changed().unwrap());
    let rows = archived_rows(&destination);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1,
        format!("archived-{}-0000000000000002", prepared.source_id)
    );
    assert_eq!(
        destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt,
        expected
    );
    let (reopened, _) = stores(&destination._root.path().join("metadata.sqlite"));
    assert_eq!(
        metadata_status(&reopened, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(expected)
    );
    assert_eq!(archived_rows(&destination), rows);
    assert_eq!(snapshot(&source.baseline), before);
}

#[test]
fn archived_reports_optional_conversion_never_claims_unsupported_completion() {
    for variant in [
        "empty",
        "unsupported_registry",
        "nonterminal",
        "unknown",
        "live_only",
    ] {
        let source = Fixture::new();
        if variant != "empty" {
            archived_report(&source, |report| match variant {
                "nonterminal" => report["outcome"] = json!("running"),
                "unknown" => report["future_field"] = json!(true),
                "live_only" => report["instance_id"] = json!("0000000000000001"),
                _ => {}
            });
        }
        if variant == "unsupported_registry" {
            let path = source.baseline.join("instances.json");
            let mut registry: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            registry["schema_version"] = json!(4);
            fs::write(path, serde_json::to_vec(&registry).unwrap()).unwrap();
        }
        let before = snapshot(&source.baseline);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let receipt = destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt;
        let expected = matches!(variant, "empty" | "live_only").then_some(0);
        assert_eq!(receipt.archived_launch_report_count, expected, "{variant}");
        assert_eq!(
            serde_json::to_value(&receipt)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("archived_launch_report_count"),
            expected.is_some()
        );
        assert!(archived_rows(&destination).is_empty());
        assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn archived_reports_publication_and_late_settings_write_are_atomic() {
    for old in [false, true] {
        for effect in [
            "ignore_report",
            "ignore_receipt",
            "rewrite_receipt",
            "rewrite_report",
            "late_settings",
        ] {
            if old && effect == "late_settings" {
                continue;
            }
            let source = Fixture::new();
            archived_report(&source, |_| {});
            let (prepared, request) = prepare(&source);
            let destination = Destination::new();
            if old {
                let mut metadata_only = prepared.clone();
                metadata_only.archived_reports = None;
                metadata_only.archived_benchmarks = None;
                destination.commit(&metadata_only, &request).unwrap();
            }
            let before_config = destination.settings.current().unwrap();
            let before_accounts = destination.accounts.snapshot().unwrap();
            let before_receipt =
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt;
            let changes = destination.settings.subscribe().unwrap();
            let action = if old {
                "UPDATE OF archived_launch_report_proof"
            } else {
                "INSERT"
            };
            let trigger = match effect {
                "ignore_report" => "BEFORE INSERT ON launch_reports BEGIN SELECT RAISE(IGNORE); END;".to_owned(),
                "ignore_receipt" => format!("BEFORE {action} ON profile_metadata_imports BEGIN SELECT RAISE(IGNORE); END;"),
                "rewrite_receipt" => format!("AFTER {action} ON profile_metadata_imports BEGIN UPDATE profile_metadata_imports SET archived_launch_report_proof=NULL; END;"),
                "rewrite_report" => format!("AFTER {action} ON profile_metadata_imports BEGIN UPDATE launch_reports SET payload=CAST('{{}}' AS BLOB); END;"),
                _ => "AFTER UPDATE ON settings_config BEGIN UPDATE launch_reports SET payload=CAST('{}' AS BLOB); END;".to_owned(),
            };
            destination
                .settings
                .metadata()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(&format!("CREATE TRIGGER corrupt_archive {trigger}"))?;
                    Ok(())
                })
                .unwrap();
            assert!(
                destination.commit(&prepared, &request).is_err(),
                "{old}/{effect}"
            );
            assert_eq!(destination.settings.current().unwrap(), before_config);
            assert_eq!(destination.accounts.snapshot().unwrap(), before_accounts);
            assert_eq!(
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt,
                before_receipt
            );
            assert!(archived_rows(&destination).is_empty());
            assert!(!changes.has_changed().unwrap());
        }
    }
}

#[test]
fn archived_reports_completed_proof_is_never_repaired_or_rebound() {
    for corruption in ["missing", "payload", "index", "source", "proof"] {
        let source = Fixture::new();
        archived_report(&source, |_| {});
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        destination.commit(&prepared, &request).unwrap();
        destination
            .settings
            .metadata()
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch(match corruption {
                    "missing" => "DELETE FROM launch_reports;",
                    "payload" => "UPDATE launch_reports SET payload=CAST('{}' AS BLOB);",
                    "index" => "UPDATE launch_reports SET instance_id='another-instance';",
                    "source" => "UPDATE profile_metadata_imports SET source_id=printf('%064d',0);",
                    _ => "UPDATE profile_metadata_imports SET archived_launch_report_proof='{}';",
                })?;
                Ok(())
            })
            .unwrap();
        let before = archived_rows(&destination);
        assert!(
            metadata_status(&destination.settings, &request.metadata_import_id).is_err(),
            "{corruption}"
        );
        assert!(
            destination.commit(&prepared, &request).is_err(),
            "{corruption}"
        );
        assert_eq!(archived_rows(&destination), before);
        assert_eq!(destination.settings.current().unwrap().revision, 1);
    }
}

#[test]
fn global_history_late_settings_write_must_roll_back_receipt_and_batch() {
    let source = Fixture::new();
    global_journal(&source, |_| {});
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let changes = destination.settings.subscribe().unwrap();
    destination
        .settings
        .metadata()
        .transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch(
                "CREATE TRIGGER corrupt_late_history AFTER UPDATE ON settings_config
            BEGIN UPDATE install_history SET payload=CAST('{}' AS BLOB); END;",
            )?;
            Ok(())
        })
        .unwrap();
    assert!(destination.commit(&prepared, &request).is_err());
    assert_eq!(destination.settings.current().unwrap().revision, 0);
    assert!(destination.accounts.snapshot().unwrap().accounts.is_empty());
    assert!(!changes.has_changed().unwrap());
    let counts = destination
        .settings
        .metadata()
        .read(|db| -> Result<_, StorageError> {
            Ok(db.query_row(
                "SELECT (SELECT count(*) FROM profile_metadata_imports),
            (SELECT count(*) FROM install_history)",
                [],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(counts, (0, 0));
    assert_eq!(snapshot(&source.baseline), before);
}

#[test]
fn global_history_optional_conversion_preserves_metadata_without_claiming_completion() {
    for corruption in ["none", "unsupported", "partial", "duplicate", "raw_unknown"] {
        let source = Fixture::new();
        if corruption != "none" {
            global_journal(&source, |journal| match corruption {
                "unsupported" => journal["entries"][0]["status"] = json!("Running"),
                "partial" => journal["entries"][1]["status"] = json!("Failed"),
                "duplicate" => journal["entries"][1] = journal["entries"][0].clone(),
                _ => journal["entries"][0]["unrecognized_effect"] = json!(true),
            });
        }
        let before = snapshot(&source.baseline);
        let inventory = source.capture();
        assert!(
            inventory.preview().metadata_import_available,
            "{corruption}"
        );
        if corruption != "none" {
            assert!(
                !inventory.preview().instances[0].ordinary_import_available,
                "{corruption}"
            );
        }
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let receipt = destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt;
        let wire = serde_json::to_value(&receipt).unwrap();
        if corruption == "none" {
            assert_eq!(wire["global_install_history_count"], 0);
            assert!(
                metadata_install_history(&destination.settings, &request.metadata_import_id, None)
                    .unwrap()
                    .records
                    .is_empty()
            );
        } else {
            assert!(
                !wire
                    .as_object()
                    .unwrap()
                    .contains_key("global_install_history_count")
            );
            assert!(
                metadata_install_history(&destination.settings, &request.metadata_import_id, None)
                    .is_err()
            );
        }
        assert_eq!(destination.accounts.snapshot().unwrap().accounts.len(), 2);
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn global_history_old_receipt_completion_preserves_later_edits_and_reopen() {
    let source = Fixture::new();
    global_journal(&source, |_| {});
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    // The preceding metadata converter committed the same accounts/preferences,
    // without global history. Exercise that real transaction with no batch.
    let mut metadata_only = prepared.clone();
    metadata_only.global_history = None;
    let original = destination
        .commit(&metadata_only, &request)
        .unwrap()
        .response
        .receipt;
    assert_eq!(original.global_install_history_count, None);
    assert!(
        metadata_install_history(&destination.settings, &request.metadata_import_id, None).is_err()
    );
    destination.accounts.select(SECOND).unwrap();
    let accounts = destination.accounts.snapshot().unwrap();
    let config = destination
        .settings
        .update(ConfigPatch {
            expected_revision: 1,
            theme: Some(ConfigTheme::Birch),
            ..ConfigPatch::default()
        })
        .unwrap();
    let changes = destination.settings.subscribe().unwrap();
    let completed = destination.commit(&prepared, &request).unwrap();
    let mut expected = original;
    expected.global_install_history_count = Some(2);
    assert!(completed.response.already_imported);
    assert_eq!(completed.response.receipt, expected);
    assert_eq!(completed.settings.config, config);
    assert_eq!(destination.accounts.snapshot().unwrap(), accounts);
    assert!(!changes.has_changed().unwrap());
    let page =
        metadata_install_history(&destination.settings, &request.metadata_import_id, None).unwrap();
    assert_eq!(page.records.len(), 2);
    assert!(
        page.records
            .iter()
            .all(|record| record.historical && record.instance_id.is_none())
    );
    assert_eq!(
        destination
            .commit(&prepared, &request)
            .unwrap()
            .response
            .receipt,
        expected
    );
    let path = destination._root.path().join("metadata.sqlite");
    let (reopened, _) = stores(&path);
    assert_eq!(
        metadata_status(&reopened, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(expected)
    );
    assert_eq!(
        metadata_install_history(&reopened, &request.metadata_import_id, None).unwrap(),
        page
    );
    assert_eq!(snapshot(&source.baseline), before);
}

#[test]
fn global_history_receipt_publication_is_exact_and_atomic_for_first_and_old_receipts() {
    for old in [false, true] {
        for effect in ["ignore", "rewrite_receipt", "rewrite_history"] {
            let source = Fixture::new();
            global_journal(&source, |_| {});
            let (prepared, request) = prepare(&source);
            let destination = Destination::new();
            if old {
                let mut metadata_only = prepared.clone();
                metadata_only.global_history = None;
                destination.commit(&metadata_only, &request).unwrap();
            }
            let before_config = destination.settings.current().unwrap();
            let before_accounts = destination.accounts.snapshot().unwrap();
            let before_receipt =
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt;
            let changes = destination.settings.subscribe().unwrap();
            let action = if old {
                "UPDATE OF global_install_history_proof"
            } else {
                "INSERT"
            };
            let (timing, statement) = match effect {
                "ignore" => ("BEFORE", "SELECT RAISE(IGNORE);"),
                "rewrite_receipt" => (
                    "AFTER",
                    "UPDATE profile_metadata_imports SET settings_revision=settings_revision+1;",
                ),
                _ => (
                    "AFTER",
                    "UPDATE install_history SET payload=CAST('{}' AS BLOB);",
                ),
            };
            destination.settings.metadata().transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch(&format!("CREATE TRIGGER corrupt_publication {timing} {action} ON profile_metadata_imports BEGIN {statement} END;"))?;
                Ok(())
            }).unwrap();
            assert!(
                destination.commit(&prepared, &request).is_err(),
                "{old}/{effect}"
            );
            assert_eq!(destination.settings.current().unwrap(), before_config);
            assert_eq!(destination.accounts.snapshot().unwrap(), before_accounts);
            assert_eq!(
                metadata_status(&destination.settings, &request.metadata_import_id)
                    .unwrap()
                    .receipt,
                before_receipt
            );
            assert!(!changes.has_changed().unwrap());
            let count = destination
                .settings
                .metadata()
                .read(|db| -> Result<u32, StorageError> {
                    Ok(db.query_row("SELECT count(*) FROM install_history", [], |row| row.get(0))?)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }
}

#[test]
fn global_history_completed_proof_is_never_repaired_or_rebound() {
    for corruption in ["missing", "changed", "source", "fingerprint", "proof"] {
        let source = Fixture::new();
        global_journal(&source, |_| {});
        let before = snapshot(&source.baseline);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        destination.commit(&prepared, &request).unwrap();
        destination
            .settings
            .metadata()
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch(match corruption {
                    "missing" => "DELETE FROM install_history;",
                    "changed" => "UPDATE install_history SET payload=CAST('{}' AS BLOB);",
                    "source" => "UPDATE profile_metadata_imports SET source_id=printf('%064d',0);",
                    "fingerprint" => {
                        "UPDATE profile_metadata_imports SET fingerprint=printf('%064d',0);"
                    }
                    _ => "UPDATE profile_metadata_imports SET global_install_history_proof='{}';",
                })?;
                Ok(())
            })
            .unwrap();
        let rows_before = destination
            .settings
            .metadata()
            .read(|db| -> Result<Vec<(String, Vec<u8>)>, StorageError> {
                Ok(db
                    .prepare("SELECT id,payload FROM install_history ORDER BY id")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert!(
            metadata_status(&destination.settings, &request.metadata_import_id).is_err(),
            "{corruption}"
        );
        assert!(
            metadata_install_history(&destination.settings, &request.metadata_import_id, None)
                .is_err(),
            "{corruption}"
        );
        assert!(
            destination.commit(&prepared, &request).is_err(),
            "{corruption}"
        );
        let rows_after = destination
            .settings
            .metadata()
            .read(|db| -> Result<Vec<(String, Vec<u8>)>, StorageError> {
                Ok(db
                    .prepare("SELECT id,payload FROM install_history ORDER BY id")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert_eq!(rows_before, rows_after, "{corruption}");
        assert_eq!(destination.settings.current().unwrap().revision, 1);
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn commits_exact_offline_identities_selection_preferences_flags_and_replacement_identity() {
    let source = Fixture::new();
    let old_identity = "b931ec93-5a28-4320-a3fb-4adf9f725b9e";
    config(&source, |value| {
        value["telemetry_enabled"] = json!(true);
        value["telemetry_install_id"] = json!(old_identity);
        value["feature_overrides"] = json!({STATE_INSPECTOR_FLAG: true});
    });
    let before = snapshot(&source.baseline);
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let mut changes = destination.settings.subscribe().unwrap();
    let committed = destination.commit(&prepared, &request).unwrap();
    assert!(!committed.response.already_imported);
    assert!(!committed.response.cutover_available);
    assert_eq!(committed.response.receipt.imported_offline_account_count, 2);
    assert_eq!(committed.response.receipt.settings_revision, 1);
    assert_eq!(committed.response.receipt.account_selection_revision, 1);
    assert_eq!(committed.settings.config.theme, ConfigTheme::Deepslate);
    assert_eq!(committed.settings.config.music_enabled, Some(false));
    assert_eq!(committed.settings.config.music_volume, Some(35));
    assert_eq!(committed.settings.config.window_width, 1280);
    assert!(committed.settings.config.onboarding_done);
    assert_ne!(
        committed.settings.telemetry_identity.as_deref(),
        Some(old_identity)
    );
    assert!(uuid::Uuid::parse_str(committed.settings.telemetry_identity.as_ref().unwrap()).is_ok());
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), committed.settings.config);
    assert!(
        destination
            .settings
            .list_flags()
            .unwrap()
            .flags
            .iter()
            .any(|flag| flag.key == STATE_INSPECTOR_FLAG && flag.enabled)
    );
    let accounts = destination.accounts.snapshot().unwrap();
    assert_eq!(
        accounts.active_account_id.as_ref().map(|id| id.as_str()),
        Some(FIRST)
    );
    assert_eq!(accounts.accounts.len(), 2);
    let original: Value =
        serde_json::from_slice(&fs::read(source.baseline.join("accounts.json")).unwrap()).unwrap();
    for input in original["accounts"].as_array().unwrap() {
        let account = accounts
            .accounts
            .iter()
            .find(|account| account.account_id.as_str() == input["account_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            account.offline_uuid.as_deref(),
            input["offline_uuid"].as_str()
        );
        assert_eq!(
            account.display_name,
            input["display_name"].as_str().unwrap()
        );
        assert_eq!(account.created_at, input["created_at"].as_str().unwrap());
        assert_eq!(account.updated_at, input["updated_at"].as_str().unwrap());
        assert_eq!(account.credential_revision, 0);
    }
    let capture = destination.accounts.capture_selected().unwrap();
    assert_eq!(capture.account_id(), FIRST);
    assert_eq!(capture.minecraft_uuid(), "92d74dda76fe332cb669d0daddfb7952");
    assert_eq!(
        metadata_status(&destination.settings, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(committed.response.receipt)
    );
    assert_eq!(snapshot(&source.baseline), before);
}

#[test]
fn second_identity_conflict_and_sql_failure_roll_back_every_owner_and_receipt() {
    let source = Fixture::new();
    let (prepared, mut request) = prepare(&source);
    let destination = Destination::new();
    destination
        .accounts
        .create_offline_account("OtherPlayer")
        .unwrap();
    request.expected_account_selection_revision =
        destination.accounts.selection_revision().unwrap();
    let accounts_before = destination.accounts.snapshot().unwrap();
    let config_before = destination.settings.current().unwrap();
    assert!(matches!(
        destination.commit(&prepared, &request),
        Err(MetadataImportError::Settings(SettingsError::Conflict))
    ));
    assert_eq!(destination.accounts.snapshot().unwrap(), accounts_before);
    assert_eq!(destination.settings.current().unwrap(), config_before);
    assert!(
        metadata_status(&destination.settings, &request.metadata_import_id)
            .unwrap()
            .receipt
            .is_none()
    );
    assert!(
        !destination
            .accounts
            .snapshot()
            .unwrap()
            .accounts
            .iter()
            .any(|account| account.account_id.as_str() == FIRST)
    );

    let empty = Destination::new();
    let changes = empty.settings.subscribe().unwrap();
    empty.settings.metadata().transaction(|transaction| -> Result<(), StorageError> {
        transaction.execute_batch("CREATE TRIGGER reject_import BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT, 'injected settings failure'); END;")?;
        Ok(())
    }).unwrap();
    request.expected_account_selection_revision = 0;
    assert!(empty.commit(&prepared, &request).is_err());
    assert!(empty.accounts.snapshot().unwrap().accounts.is_empty());
    assert_eq!(empty.settings.current().unwrap().revision, 0);
    assert!(empty.settings.telemetry_identity().unwrap().is_none());
    assert!(!changes.has_changed().unwrap());
    assert!(
        metadata_status(&empty.settings, &request.metadata_import_id)
            .unwrap()
            .receipt
            .is_none()
    );
    empty
        .settings
        .metadata()
        .transaction(|transaction| -> Result<(), StorageError> {
            transaction.execute_batch("DROP TRIGGER reject_import")?;
            Ok(())
        })
        .unwrap();
    assert!(
        !empty
            .commit(&prepared, &request)
            .unwrap()
            .response
            .already_imported
    );
}

#[test]
fn stale_destination_revisions_cancelled_work_and_changed_source_publish_nothing() {
    let source = Fixture::new();
    let (prepared, request) = prepare(&source);
    let destination = Destination::new();
    let mut stale = request.clone();
    stale.expected_settings_revision = 1;
    assert!(matches!(
        destination.commit(&prepared, &stale),
        Err(MetadataImportError::Settings(SettingsError::Conflict))
    ));
    stale = request.clone();
    stale.expected_account_selection_revision = 1;
    assert!(matches!(
        destination.commit(&prepared, &stale),
        Err(MetadataImportError::Settings(SettingsError::Conflict))
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        prepared.commit(
            &destination.settings,
            &destination.accounts,
            &destination.library.admit_application_root().unwrap(),
            &request,
            &cancelled
        ),
        Err(MetadataImportError::Source(ImportError::Cancelled))
    ));
    config(&source, |value| value["music_volume"] = json!(36));
    assert!(matches!(
        destination.commit(&prepared, &request),
        Err(MetadataImportError::Source(ImportError::SourceChanged))
    ));
    assert!(destination.accounts.snapshot().unwrap().accounts.is_empty());
    assert_eq!(destination.settings.current().unwrap().revision, 0);
    assert!(
        metadata_status(&destination.settings, &request.metadata_import_id)
            .unwrap()
            .receipt
            .is_none()
    );
}

#[test]
fn restart_reimport_preserves_later_edits_and_changed_fingerprint_cannot_reapply() {
    let source = Fixture::new();
    let (prepared, request) = prepare(&source);
    let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let library = match LibraryLifecycle::open(root.path()) {
        LibraryOpenOutcome::Ready(library) => library,
        other => panic!("isolated restart root: {other:?}"),
    };
    let pin = library.admit_application_root().unwrap();
    let path = root.path().join("metadata.sqlite");
    let original_receipt;
    let edited_accounts;
    let edited_config;
    let replacement_identity;
    {
        let (settings, accounts) = stores(&path);
        original_receipt = prepared
            .commit(
                &settings,
                &accounts,
                &pin,
                &request,
                &CancellationToken::new(),
            )
            .unwrap()
            .response
            .receipt;
        accounts.select(SECOND).unwrap();
        edited_accounts = accounts.snapshot().unwrap();
        edited_config = settings
            .update(ConfigPatch {
                expected_revision: 1,
                theme: Some(ConfigTheme::Birch),
                telemetry_enabled: Some(true),
                ..ConfigPatch::default()
            })
            .unwrap();
        replacement_identity = settings.telemetry_identity().unwrap();
    }
    let (settings, accounts) = stores(&path);
    let (readmitted, retry) = prepare(&source);
    assert_eq!(retry.metadata_import_id, request.metadata_import_id);
    let repeated = readmitted
        .commit(
            &settings,
            &accounts,
            &pin,
            &retry,
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(repeated.response.already_imported);
    assert_eq!(repeated.response.receipt, original_receipt);
    assert_eq!(repeated.settings.config, edited_config);
    assert_eq!(repeated.settings.telemetry_identity, replacement_identity);
    assert_eq!(accounts.snapshot().unwrap(), edited_accounts);
    config(&source, |value| value["music_volume"] = json!(36));
    let (changed, changed_request) = prepare(&source);
    assert_ne!(
        changed_request.metadata_import_id,
        request.metadata_import_id
    );
    assert!(matches!(
        changed.commit(
            &settings,
            &accounts,
            &pin,
            &changed_request,
            &CancellationToken::new()
        ),
        Err(MetadataImportError::Settings(SettingsError::Conflict))
    ));
    assert_eq!(settings.current().unwrap(), edited_config);
    assert_eq!(accounts.snapshot().unwrap(), edited_accounts);
    // No source authority is needed to resolve a committed response.
    drop((changed, readmitted, prepared, source));
    assert_eq!(
        metadata_status(&settings, &request.metadata_import_id)
            .unwrap()
            .receipt,
        Some(original_receipt)
    );
}

#[test]
fn converter_availability_rejects_microsoft_unknown_preferences_and_duplicate_ids() {
    for corruption in ["online", "unknown_setting", "duplicate_identity"] {
        let source = Fixture::new();
        match corruption {
            "online" => config(&source, |value| value["launch_auth_mode"] = json!("online")),
            "unknown_setting" => config(&source, |value| value["future_preference"] = json!(true)),
            _ => {
                let path = source.baseline.join("accounts.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                let duplicate = value["accounts"][0].clone();
                value["accounts"].as_array_mut().unwrap().push(duplicate);
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        let before = snapshot(&source.baseline);
        let previews = ImportPreviews::new();
        let preview = previews.admit(source.capture()).unwrap();
        assert!(!preview.metadata_import_available, "{corruption}");
        assert!(!preview.cutover_available);
        assert!(matches!(
            previews.prepare_metadata(&preview.fingerprint),
            Err(ImportError::InvalidData)
        ));
        assert_eq!(snapshot(&source.baseline), before);
    }
}

const MICROSOFT_SOURCE: &str = "microsoft-msa-1111111111111111-2222222222222222";
const MICROSOFT_DESTINATION: &str = "01234567-89ab-cdef-0123-456789abcdef";

fn microsoft_source(source: &Fixture, name: &str, mixed: bool) {
    let path = source.baseline.join("accounts.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    if !mixed {
        value["accounts"] = json!([]);
    }
    value["accounts"].as_array_mut().unwrap().push(json!({
        "account_id": MICROSOFT_SOURCE,
        "kind":"microsoft", "display_name":name,
        "login_id":"msa-3333333333333333-4444444444444444",
        "minecraft_profile_id":"0123456789abcdef0123456789abcdef",
        "created_at":"2026-09-08T00:02:00Z", "updated_at":"2026-09-08T00:03:00Z"
    }));
    value["active_account_id"] = json!(MICROSOFT_SOURCE);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    config(source, |value| {
        value["launch_auth_mode"] = json!("online");
        value["username"] = json!(name);
    });
}

fn authenticate(accounts: &AccountDirectory) {
    accounts
        .commit_microsoft(
            accounts.selection_revision().unwrap(),
            crate::accounts::model::MicrosoftIdentity {
                login_id: "cefb15e1-197e-48b3-9fd3-8b5aeb43a534".into(),
                profile_id: MICROSOFT_DESTINATION.into(),
                display_name: "Authenticated".into(),
                credential_revision: 1,
                profile: crate::accounts::microsoft::MinecraftProfile {
                    id: MICROSOFT_DESTINATION.into(),
                    name: "Authenticated".into(),
                    skins: vec![],
                    capes: vec![],
                },
            },
        )
        .unwrap();
}

#[test]
fn microsoft_only_and_mixed_profiles_preserve_mapping_names_and_unauthenticated_selection() {
    for (name, mixed) in [("A", false), ("Ab", true)] {
        let source = Fixture::new();
        microsoft_source(&source, name, mixed);
        let before = snapshot(&source.baseline);
        let preview = source.capture().preview();
        assert!(preview.metadata_import_available);
        assert!(preview.instances[0].ordinary_import_available);
        assert!(!preview.cutover_available);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let committed = destination.commit(&prepared, &request).unwrap();
        let receipt = &committed.response.receipt;
        assert_eq!(
            receipt.imported_offline_account_count,
            if mixed { 2 } else { 0 }
        );
        assert_eq!(receipt.imported_microsoft_account_count, 1);
        let mapping = receipt.account_id_mapping.as_ref().unwrap();
        assert_eq!(mapping[MICROSOFT_SOURCE], MICROSOFT_DESTINATION);
        if mixed {
            assert_eq!(mapping[FIRST], FIRST);
            assert_eq!(mapping[SECOND], SECOND);
        }
        assert_eq!(committed.settings.config.username, name);
        assert_eq!(
            committed.settings.config.launch_auth_mode,
            ConfigLaunchAuthMode::Online
        );
        let snapshot = destination.accounts.snapshot().unwrap();
        let active = snapshot.active_account().unwrap();
        assert_eq!(active.account_id.as_str(), MICROSOFT_DESTINATION);
        assert_eq!(active.created_at, "2026-09-08T00:02:00Z");
        assert_eq!(active.updated_at, "2026-09-08T00:03:00Z");
        assert!(active.login_id.is_none());
        assert!(active.minecraft_profile.is_none());
        assert_eq!(active.credential_revision, 0);
        assert_eq!(
            snapshot.launch_auth_mode,
            crate::accounts::model::LaunchAuthMode::Online
        );
        assert_eq!(
            metadata_status(&destination.settings, &request.metadata_import_id)
                .unwrap()
                .receipt
                .as_ref(),
            Some(receipt)
        );
        assert_eq!(super::super::tests::snapshot(&source.baseline), before);
        // The ordinary offline command remains stricter than imported provider names.
        assert!(
            destination
                .settings
                .update(ConfigPatch {
                    expected_revision: 1,
                    username: Some(name.into()),
                    ..ConfigPatch::default()
                })
                .is_err()
        );
    }
}

#[test]
fn mixed_import_does_not_downgrade_authenticated_collision_and_replay_preserves_reauthentication() {
    let source = Fixture::new();
    microsoft_source(&source, "Ab", true);
    let (prepared, mut request) = prepare(&source);
    let collision = Destination::new();
    authenticate(&collision.accounts);
    let before = collision.accounts.snapshot().unwrap();
    request.expected_account_selection_revision = before.selection_revision;
    assert!(matches!(
        collision.commit(&prepared, &request),
        Err(MetadataImportError::Settings(SettingsError::Conflict))
    ));
    assert_eq!(collision.accounts.snapshot().unwrap(), before);
    assert_eq!(collision.settings.current().unwrap().revision, 0);
    assert!(
        metadata_status(&collision.settings, &request.metadata_import_id)
            .unwrap()
            .receipt
            .is_none()
    );

    let destination = Destination::new();
    request.expected_account_selection_revision = 0;
    let receipt = destination
        .commit(&prepared, &request)
        .unwrap()
        .response
        .receipt;
    authenticate(&destination.accounts);
    let authenticated = destination.accounts.snapshot().unwrap();
    let edited = destination
        .settings
        .update(ConfigPatch {
            expected_revision: 1,
            theme: Some(ConfigTheme::Birch),
            ..ConfigPatch::default()
        })
        .unwrap();
    let path = destination._root.path().join("metadata.sqlite");
    let Destination {
        accounts,
        settings,
        library,
        _root,
    } = destination;
    drop((accounts, settings));
    let (settings, accounts) = stores(&path);
    let (readmitted, _) = prepare(&source);
    let result = readmitted
        .commit(
            &settings,
            &accounts,
            &library.admit_application_root().unwrap(),
            &request,
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(result.response.already_imported);
    assert_eq!(result.response.receipt, receipt);
    assert_eq!(result.settings.config, edited);
    assert_eq!(accounts.snapshot().unwrap(), authenticated);
    assert_eq!(
        accounts
            .snapshot()
            .unwrap()
            .active_account()
            .unwrap()
            .credential_revision,
        1
    );
}

#[test]
fn malformed_microsoft_rows_and_canonical_duplicates_stay_explicitly_blocked() {
    for corruption in [
        "nil",
        "legacy_id",
        "login",
        "token",
        "duplicate",
        "selection",
    ] {
        let source = Fixture::new();
        microsoft_source(&source, "Ab", false);
        let path = source.baseline.join("accounts.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match corruption {
            "nil" => {
                value["accounts"][0]["minecraft_profile_id"] =
                    json!("00000000000000000000000000000000")
            }
            "legacy_id" => value["accounts"][0]["account_id"] = json!("microsoft-arbitrary"),
            "login" => value["accounts"][0]["login_id"] = json!("not-a-legacy-login"),
            "token" => value["accounts"][0]["access_token"] = json!("must-not-transfer"),
            "selection" => value["active_account_id"] = json!("microsoft-msa-1-2"),
            _ => {
                let mut duplicate = value["accounts"][0].clone();
                duplicate["account_id"] = json!("microsoft-msa-1-2");
                duplicate["minecraft_profile_id"] = json!(MICROSOFT_DESTINATION.to_uppercase());
                value["accounts"].as_array_mut().unwrap().push(duplicate);
            }
        }
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        let before = snapshot(&source.baseline);
        let previews = ImportPreviews::new();
        let preview = previews.admit(source.capture()).unwrap();
        assert!(!preview.metadata_import_available, "{corruption}");
        assert!(
            !preview.instances[0].ordinary_import_available,
            "{corruption}"
        );
        assert!(
            preview
                .blockers
                .contains(&super::super::ImportBlocker::AccountRequiresConversion)
        );
        assert!(previews.prepare_metadata(&preview.fingerprint).is_err());
        assert_eq!(snapshot(&source.baseline), before);
    }
}

#[test]
fn v1_receipts_keep_historical_meaning_and_new_mapping_corruption_is_rejected() {
    let old_store = Arc::new(MetadataStore::in_memory().unwrap());
    old_store.migrate(&[METADATA_IMPORT_MIGRATION]).unwrap();
    let old_settings = SettingsStore::new(Arc::clone(&old_store)).unwrap();
    // A v1 receipt had no Microsoft count or mapping. Migrated rows retain
    // those defaults instead of claiming that a source was re-read or mapped.
    let old_id = "a".repeat(64);
    old_store.transaction(|transaction| -> Result<(), StorageError> {
        transaction.execute("INSERT INTO profile_metadata_imports(source_id,fingerprint,import_id,account_count,settings_revision,selection_revision) VALUES('old-source',?1,?1,2,3,2)", [&old_id])?;
        Ok(())
    }).unwrap();
    old_store
        .migrate(&[
            METADATA_IMPORT_IDENTITIES_MIGRATION,
            METADATA_IMPORT_HISTORY_MIGRATION,
            METADATA_IMPORT_ARCHIVED_REPORTS_MIGRATION,
            METADATA_IMPORT_ARCHIVED_BENCHMARKS_MIGRATION,
            METADATA_IMPORT_ARCHIVED_OPERATIONS_MIGRATION,
        ])
        .unwrap();
    let receipt = metadata_status(&old_settings, &old_id)
        .unwrap()
        .receipt
        .unwrap();
    assert_eq!(receipt.imported_offline_account_count, 2);
    assert_eq!(receipt.imported_microsoft_account_count, 0);
    assert_eq!(receipt.account_id_mapping, None);
    assert_eq!(receipt.settings_revision, 3);
    assert_eq!(receipt.global_install_history_count, None);
    assert_eq!(receipt.archived_launch_report_count, None);
    assert_eq!(receipt.archived_benchmark_count, None);

    let destination = Destination::new();
    let source = Fixture::new();
    microsoft_source(&source, "Ab", true);
    let (prepared, request) = prepare(&source);
    destination.commit(&prepared, &request).unwrap();
    for corrupted in [
        json!([
            [FIRST, FIRST],
            [FIRST, FIRST],
            [MICROSOFT_SOURCE, MICROSOFT_DESTINATION]
        ]),
        json!([
            [FIRST, FIRST],
            [SECOND, SECOND],
            [MICROSOFT_SOURCE, MICROSOFT_DESTINATION.to_uppercase()]
        ]),
        json!([[FIRST, FIRST]]),
    ] {
        destination
            .settings
            .metadata()
            .transaction(|transaction| -> Result<(), StorageError> {
                transaction.execute(
                    "UPDATE profile_metadata_imports SET account_id_mapping=?1 WHERE import_id=?2",
                    params![corrupted.to_string(), request.metadata_import_id],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            metadata_status(&destination.settings, &request.metadata_import_id),
            Err(MetadataImportError::Settings(SettingsError::Corrupt))
        ));
    }
}

#[tokio::test]
async fn ordinary_copy_accepts_mixed_identity_profile_without_relaxing_retained_payload_blockers() {
    let source = Fixture::new();
    microsoft_source(&source, "Ab", true);
    let before = snapshot(&source.baseline);
    let previews = ImportPreviews::new();
    let preview = previews.admit(source.capture()).unwrap();
    let prepared = previews
        .prepare_instance(&preview.fingerprint, "0000000000000001")
        .unwrap();
    let (_root, service) = crate::instances::create::tests::fixture();
    let imported = service
        .import_instance(prepared)
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let path = service
        .directories()
        .library()
        .admit()
        .unwrap()
        .read_projection()
        .unwrap()
        .join("instances")
        .join(imported.id.as_str());
    assert_eq!(
        fs::read(path.join("options.txt")).unwrap(),
        fs::read(
            source
                .baseline
                .join("instances/0000000000000001/options.txt")
        )
        .unwrap()
    );
    assert_eq!(snapshot(&source.baseline), before);
    fs::create_dir_all(source.baseline.join("benchmarks")).unwrap();
    fs::write(
        source.baseline.join("benchmarks/retained.json"),
        br#"{"history":[]}"#,
    )
    .unwrap();
    let blocked = source.capture().preview();
    assert!(blocked.metadata_import_available);
    assert!(!blocked.instances[0].ordinary_import_available);
    assert!(
        blocked
            .blockers
            .contains(&super::super::ImportBlocker::RetainedHistoryRequiresConversion)
    );
    assert!(!blocked.cutover_available);
}
