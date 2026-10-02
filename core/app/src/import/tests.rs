use super::*;
use axial_fs::{RootSession, RootSessionAcquireOutcome};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

const FIRST: &str = "0000000000000001";
const SECOND: &str = "0000000000000002";

pub(crate) struct Fixture {
    source: ReadOnlySource,
    owner: RootSession,
    pub(crate) baseline: PathBuf,
    root: tempfile::TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        // Production admission rejects symlinked ancestors, including macOS's
        // /var alias. Create fixtures under the physical temporary directory.
        let temporary_parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = tempfile::tempdir_in(temporary_parent).unwrap();
        let baseline = root.path().join("baseline");
        let replacement = root.path().join("replacement");
        fs::create_dir(&baseline).unwrap();
        fs::create_dir(&replacement).unwrap();
        copy_fixture(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../acceptance/fixtures/profiles/offline-vanilla"),
            &baseline,
        );
        let owner = match RootSession::acquire(&replacement) {
            RootSessionAcquireOutcome::Acquired(owner) => owner,
            other => panic!("isolated replacement root: {other:?}"),
        };
        let source = ReadOnlySource::from_admitted_directory(
            owner.admit_absolute_directory(&baseline).unwrap(),
        );
        Self {
            source,
            owner,
            baseline,
            root,
        }
    }

    pub(crate) fn capture(&self) -> Inventory {
        self.try_capture().unwrap()
    }

    pub(crate) fn try_capture(&self) -> ImportResult<Inventory> {
        Inventory::capture(&self.source, &BTreeMap::new())
    }
    fn write(&self, relative: &str, value: &Value) {
        let path = self.baseline.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn record(&self, relative: &str) -> Value {
        serde_json::from_slice(&fs::read(self.baseline.join(relative)).unwrap()).unwrap()
    }
    fn two_instances(&self) {
        let mut registry = self.record("instances.json");
        let mut second = registry["instances"][0].clone();
        second["id"] = json!(SECOND);
        second["name"] = json!("Second");
        registry["instances"].as_array_mut().unwrap().push(second);
        self.write("instances.json", &registry);
        fs::create_dir(self.baseline.join("instances").join(SECOND)).unwrap();
    }
}

fn copy_fixture(source: &Path, destination: &Path) {
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&target).unwrap();
            copy_fixture(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A canary comparison includes inode/link count as well as bytes, so rewriting
/// a source with identical content cannot make the no-mutation assertion pass.
pub(crate) fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u64, u64)> {
    fn visit(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, (Vec<u8>, u64, u64)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (metadata.ino(), metadata.nlink())
        };
        #[cfg(not(unix))]
        let identity = (0, 0);
        let bytes = if metadata.is_file() {
            fs::read(path).unwrap()
        } else if metadata.is_symlink() {
            fs::read_link(path)
                .unwrap()
                .to_string_lossy()
                .as_bytes()
                .to_vec()
        } else {
            vec![]
        };
        output.insert(
            path.strip_prefix(root).unwrap().into(),
            (bytes, identity.0, identity.1),
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), output);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn preview_is_repeatable_and_never_mutates_or_claims_cutover() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.baseline);
    let first = fixture.capture();
    let second = fixture.capture();
    assert_eq!(first.preview(), second.preview());
    assert_eq!(before, snapshot(&fixture.baseline));
    assert!(!fixture.baseline.join(".axial-root.lease").exists());
    assert_eq!(first.preview().offline_account_count, 2);
    assert_eq!(first.preview().instances[0].loader_key, "vanilla");
    assert!(!first.preview().cutover_available);
    assert!(
        first
            .preview()
            .blockers
            .contains(&ImportBlocker::BrowserPreferencesRequired)
    );
    assert!(
        first
            .preview()
            .blockers
            .contains(&ImportBlocker::CutoverNotImplemented)
    );
    assert!(
        !first
            .preview()
            .blockers
            .contains(&ImportBlocker::RetainedPreferenceRequiresConversion)
    );
    assert_eq!(
        first.record_bytes("profile/config.json").unwrap(),
        fs::read(fixture.baseline.join("config.json")).unwrap()
    );
}

#[test]
fn unresolved_records_preserve_exact_targets_and_terminal_effects() {
    let fixture = Fixture::new();
    fixture.two_instances();
    let mut registry = fixture.record("instances.json");
    registry["instances"][0]["loader_key"] = json!("future-loader");
    registry["pending_deletions"] = json!([{ "instance_id": FIRST, "delete_files": true, "park": "/private/exact-obligation" }]);
    fixture.write("instances.json", &registry);
    let entry = json!({ "status": "Succeeded", "targets": [{ "kind": "Instance", "id": SECOND }],
        "retained_effect": { "path": "/private/secret-obligation", "before_sha256": "required-original-hash" } });
    fixture.write(
        "state/operation-journals.json",
        &json!({ "schema": "axial.state.operation_journals.v10", "entries": [entry] }),
    );
    let inventory = fixture.capture();
    let preview = inventory.preview();
    assert_eq!(preview.instances[0].loader_key, "future-loader");
    assert!(
        preview.instances[0]
            .blockers
            .contains(&ImportBlocker::UnsupportedLoader)
    );
    assert!(
        preview.instances[0]
            .blockers
            .contains(&ImportBlocker::PendingDeletion)
    );
    assert!(
        !preview.instances[0]
            .blockers
            .contains(&ImportBlocker::UnsettledOperation)
    );
    assert!(
        !preview.instances[1]
            .blockers
            .contains(&ImportBlocker::PendingDeletion)
    );
    assert!(
        preview.instances[1]
            .blockers
            .contains(&ImportBlocker::UnsettledOperation)
    );
    let obligation = inventory
        .obligations()
        .iter()
        .find(|record| record.source_record.ends_with("#/entries/0"))
        .unwrap();
    assert_eq!(obligation.original, Some(entry));
    assert_eq!(obligation.instance_ids, vec![SECOND.to_owned()]);
    let public = serde_json::to_string(&preview).unwrap();
    assert!(!public.contains("/private/"));
    assert!(!public.contains("required-original-hash"));
    assert!(!public.contains("source_record"));
}

#[test]
fn malformed_managed_intents_and_history_cannot_be_lost_as_success() {
    let fixture = Fixture::new();
    fixture.two_instances();
    fixture.write(
        "performance/rules-cache.json",
        &json!({ "signature": "unverified" }),
    );
    fixture.write(
        "benchmarks/launch/retained.json",
        &json!({ "instance_id": SECOND, "results": [1, 2] }),
    );
    let intent = format!("instances/{FIRST}/mods/.axial-lock.json.delete.intent");
    fs::write(fixture.baseline.join(&intent), b"{interrupted-json").unwrap();
    let inventory = fixture.capture();
    assert!(
        inventory
            .preview()
            .blockers
            .contains(&ImportBlocker::ManagedStateRequiresConversion)
    );
    assert!(
        inventory
            .preview()
            .blockers
            .contains(&ImportBlocker::RetainedHistoryRequiresConversion)
    );
    assert!(
        inventory.preview().instances[0]
            .blockers
            .contains(&ImportBlocker::UnsupportedSchema)
    );
    assert!(
        !inventory.preview().instances[1]
            .blockers
            .contains(&ImportBlocker::UnsupportedSchema)
    );
    assert_eq!(
        inventory.record_bytes(&intent).unwrap(),
        b"{interrupted-json"
    );
    assert!(
        inventory
            .obligations()
            .iter()
            .any(|record| record.source_record == intent && record.original.is_none())
    );
}

#[test]
fn content_provenance_and_nested_performance_snapshots_require_conversion() {
    let fixture = Fixture::new();
    let content = format!("instances/{FIRST}/axial.content.json");
    let performance = format!("instances/{FIRST}/mods/.axial-performance/rollback/snapshot.json");
    let manifest = json!({ "schema_version": 3, "entries": [{ "filename": "user-owned.jar" }] });
    let snapshot = json!({ "files": [{ "before": "keep-exact-proof" }] });
    fixture.write(&content, &manifest);
    fixture.write(&performance, &snapshot);
    let inventory = fixture.capture();
    let blockers = &inventory.preview().instances[0].blockers;
    assert!(blockers.contains(&ImportBlocker::ContentProvenanceRequiresConversion));
    assert!(blockers.contains(&ImportBlocker::ManagedStateRequiresConversion));
    assert_eq!(
        inventory
            .obligations()
            .iter()
            .find(|record| record.source_record == content)
            .unwrap()
            .original,
        Some(manifest)
    );
    assert_eq!(
        inventory
            .obligations()
            .iter()
            .find(|record| record.source_record == performance)
            .unwrap()
            .original,
        Some(snapshot)
    );
}

#[test]
fn changed_bytes_replacements_and_new_records_invalidate_captured_preview() {
    for change in 0..3 {
        let fixture = Fixture::new();
        let inventory = fixture.capture();
        let target = fixture.baseline.join("config.json");
        match change {
            0 => fs::write(target, b"changed source").unwrap(),
            1 => {
                let bytes = fs::read(&target).unwrap();
                fs::rename(&target, fixture.root.path().join("old-config.json")).unwrap();
                fs::write(&target, bytes).unwrap();
            }
            _ => fixture.write("new-retained.json", &json!({ "must_not_be_missed": true })),
        }
        assert!(matches!(
            inventory.revalidate(),
            Err(ImportError::SourceChanged)
        ));
    }
}

#[test]
fn serialized_external_library_path_does_not_admit_it() {
    let fixture = Fixture::new();
    let external = fixture.root.path().join("external");
    fs::rename(fixture.baseline.join("instances"), &external).unwrap();
    let before = snapshot(&external);
    let mut config = fixture.record("config.json");
    config["library_dir"] = json!(external.to_string_lossy());
    config["library_mode"] = json!("existing");
    fixture.write("config.json", &config);
    let preview = fixture.capture().preview();
    assert!(
        preview.instances[0]
            .blockers
            .contains(&ImportBlocker::MissingInstanceSource)
    );
    let admitted = ReadOnlySource::from_admitted_directory(
        fixture
            .owner
            .admit_absolute_directory(&external.join(FIRST))
            .unwrap(),
    );
    let inventory = Inventory::capture(
        &fixture.source,
        &BTreeMap::from([(FIRST.to_owned(), admitted)]),
    )
    .unwrap();
    assert!(
        !inventory.preview().instances[0]
            .blockers
            .contains(&ImportBlocker::MissingInstanceSource)
    );
    assert_eq!(before, snapshot(&external));
}

#[test]
fn cancelled_or_oversized_preview_has_no_file_effects() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.baseline);
    let cancelled = Inventory::capture_controlled(
        &fixture.source,
        &BTreeMap::new(),
        CaptureLimits::default(),
        Arc::new(AtomicBool::new(true)),
    );
    assert!(matches!(cancelled, Err(ImportError::Cancelled)));
    let too_large = Inventory::capture_controlled(
        &fixture.source,
        &BTreeMap::new(),
        CaptureLimits {
            files: 100,
            bytes: 1,
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(too_large, Err(ImportError::LimitExceeded)));
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[test]
fn total_file_budget_is_not_reset_between_directory_listings() {
    let fixture = Fixture::new();
    for group in 0..3 {
        for file in 0..4 {
            fixture.write(
                &format!("instances/{FIRST}/group-{group}/record-{file}.json"),
                &json!({ "retained": true }),
            );
        }
    }
    let before = snapshot(&fixture.baseline);
    let result = Inventory::capture_controlled(
        &fixture.source,
        &BTreeMap::new(),
        CaptureLimits {
            files: 12,
            ..CaptureLimits::default()
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(result, Err(ImportError::LimitExceeded)));
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[test]
fn retained_settings_and_unknown_state_block_without_erasing_records() {
    let fixture = Fixture::new();
    let mut config = fixture.record("config.json");
    config["retained_future_preference"] = json!("must survive");
    fixture.write("config.json", &config);
    fixture.write(
        "state/future-state.json",
        &json!({ "instance_id": FIRST, "obligation": "must survive" }),
    );
    let inventory = fixture.capture();
    assert!(
        inventory
            .preview()
            .blockers
            .contains(&ImportBlocker::RetainedPreferenceRequiresConversion)
    );
    assert!(
        inventory
            .preview()
            .blockers
            .contains(&ImportBlocker::UnknownRetainedRecord)
    );
    assert_eq!(
        inventory
            .obligations()
            .iter()
            .find(|record| record.source_record == "profile/config.json")
            .unwrap()
            .original,
        Some(config)
    );
}

fn guardian_rejection_streak_snapshot(count: usize) -> Value {
    json!({
        "schema": "axial.state.persisted_state_rejection_streaks.v1",
        "entries": (0..count).map(|index| json!({
            "store": "benchmark_suite_driver",
            "record_id": format!("benchmark-suite-driver-{index:016x}"),
            "physical_identity": format!("sha256.{}", ["01234567"; 8].join(".")),
            "consecutive_startups": index % 3 + 1,
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn guardian_rejection_streaks_allow_import_without_adopting_eligibility() {
    for count in [0, 1, 8] {
        let fixture = Fixture::new();
        let path = "state/persisted-state-rejection-streaks.json";
        let mut record = guardian_rejection_streak_snapshot(count);
        if count == 8 {
            record["entries"][7]["record_id"] = json!("benchmark-suite-driver-ffffffffffffffff");
        }
        fixture.write(path, &record);
        let mut bytes = fs::read(fixture.baseline.join(path)).unwrap();
        if count == 8 {
            bytes.resize(32 * 1024, b' ');
            fs::write(fixture.baseline.join(path), &bytes).unwrap();
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(preview.instances[0].ordinary_import_available, "{count}");
        assert!(!preview.cutover_available);
        assert!(inventory.obligations().is_empty());
        assert_eq!(
            inventory.record_bytes(&format!("profile/{path}")).unwrap(),
            bytes
        );
        let prepared = inventory
            .prepare_instance(&preview.fingerprint, FIRST)
            .unwrap();
        assert_eq!(prepared.instance().version_id, "1.20.1");
        prepared.revalidate().unwrap();
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn guardian_rejection_streaks_reject_invalid_original_records() {
    for invalid in [
        "schema",
        "envelope-field",
        "entry-field",
        "missing-field",
        "store",
        "id",
        "identity",
        "low-count",
        "high-count",
        "entries-limit",
        "duplicate-entry",
        "entry-order",
        "duplicate-field",
        "malformed",
        "bytes-limit",
    ] {
        let fixture = Fixture::new();
        let path = "state/persisted-state-rejection-streaks.json";
        let mut record = guardian_rejection_streak_snapshot(2);
        match invalid {
            "schema" => {
                record["schema"] = json!("axial.state.persisted_state_rejection_streaks.v2")
            }
            "envelope-field" => record["effect"] = json!("retain"),
            "entry-field" => record["entries"][0]["effect"] = json!("retain"),
            "missing-field" => {
                record["entries"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("physical_identity");
            }
            "store" => record["entries"][0]["store"] = json!("installation"),
            "id" => {
                record["entries"][0]["record_id"] = json!("benchmark-suite-driver-000000000000000A")
            }
            "identity" => record["entries"][0]["physical_identity"] = json!("sha256.01234567"),
            "low-count" => record["entries"][0]["consecutive_startups"] = json!(0),
            "high-count" => record["entries"][0]["consecutive_startups"] = json!(4),
            "entries-limit" => record = guardian_rejection_streak_snapshot(9),
            "duplicate-entry" => record["entries"][1] = record["entries"][0].clone(),
            "entry-order" => record["entries"].as_array_mut().unwrap().reverse(),
            "duplicate-field" | "malformed" | "bytes-limit" => {}
            _ => unreachable!(),
        }
        fixture.write(path, &record);
        let mut bytes = serde_json::to_vec(&record).unwrap();
        match invalid {
            "duplicate-field" => {
                let original = String::from_utf8(bytes).unwrap();
                bytes = original
                    .replacen(
                        "\"consecutive_startups\":1",
                        "\"consecutive_startups\":1,\"consecutive_startups\":1",
                        1,
                    )
                    .into_bytes();
                assert_ne!(bytes, original.as_bytes());
            }
            "malformed" => bytes = b"{".to_vec(),
            "bytes-limit" => bytes.resize(32 * 1024 + 1, b' '),
            _ => {}
        }
        fs::write(fixture.baseline.join(path), &bytes).unwrap();
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(!preview.instances[0].ordinary_import_available, "{invalid}");
        assert!(
            preview.blockers.contains(&ImportBlocker::UnsupportedSchema),
            "{invalid}"
        );
        assert!(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(
            inventory.record_bytes(&format!("profile/{path}")).unwrap(),
            bytes
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn guardian_rejection_streaks_remain_in_the_source_fence() {
    let fixture = Fixture::new();
    let path = "state/persisted-state-rejection-streaks.json";
    let mut record = guardian_rejection_streak_snapshot(1);
    fixture.write(path, &record);
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(inventory.fingerprint(), FIRST)
        .unwrap();
    record["entries"][0]["consecutive_startups"] = json!(2);
    fixture.write(path, &record);
    let before = snapshot(&fixture.baseline);
    assert!(matches!(
        inventory.revalidate(),
        Err(ImportError::SourceChanged)
    ));
    assert!(matches!(
        prepared.revalidate(),
        Err(ImportError::SourceChanged)
    ));
    assert!(matches!(
        inventory.prepare_instance(inventory.fingerprint(), FIRST),
        Err(ImportError::SourceChanged)
    ));
    let refreshed = fixture.capture();
    assert_ne!(refreshed.fingerprint(), inventory.fingerprint());
    assert!(refreshed.preview().instances[0].ordinary_import_available);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[test]
fn guardian_rejection_streaks_do_not_waive_retained_effects_or_unknown_records() {
    for (other, blocker) in [
        ("unknown", ImportBlocker::UnknownRetainedRecord),
        ("deletion", ImportBlocker::PendingDeletion),
        ("journal", ImportBlocker::UnsettledOperation),
    ] {
        let fixture = Fixture::new();
        fixture.write(
            "state/persisted-state-rejection-streaks.json",
            &guardian_rejection_streak_snapshot(1),
        );
        match other {
            "unknown" => fixture.write(
                "state/other.json",
                &json!({"instance_id": FIRST, "effect": "preserve"}),
            ),
            "deletion" => {
                let mut registry = fixture.record("instances.json");
                registry["pending_deletions"] =
                    json!([{"instance_id": FIRST, "delete_files": true}]);
                fixture.write("instances.json", &registry);
            }
            "journal" => {
                let mut journal = terminal_performance_journal();
                journal["entries"][0]["status"] = json!("Running");
                journal["entries"][0]["outcome"] = Value::Null;
                fixture.write("state/operation-journals.json", &journal);
            }
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(preview.blockers.contains(&blocker), "{other}");
        assert!(!preview.instances[0].ordinary_import_available, "{other}");
        assert!(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .is_err(),
            "{other}"
        );
        assert!(
            !inventory
                .obligations()
                .iter()
                .any(|record| record.source_record
                    == "profile/state/persisted-state-rejection-streaks.json")
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

const USER_MOD_WITNESS_PATH: &str = "guardian-user-mod-witnesses.json";

fn guardian_user_mod_witness_snapshot() -> Value {
    let entry = json!({"digest": "a".repeat(64), "size": u64::MAX, "modified_at_ns": u64::MAX});
    json!({
        "schema": "axial.guardian_user_mod_witnesses", "schema_version": 1,
        "witnesses": [
            {"instance_id": FIRST, "instance_created_at": "2024-02-29T12:34:56.123456+02:00",
                "entries": [entry.clone(), entry]},
            {"instance_id": "ffffffffffffffff", "instance_created_at": "2020-01-01T00:00:00Z", "entries": []}
        ]
    })
}

#[tokio::test]
async fn guardian_user_mod_witness_import_copies_instances_without_guardian_state() {
    let fixture = Fixture::new();
    fixture.write(USER_MOD_WITNESS_PATH, &guardian_user_mod_witness_snapshot());
    let before = snapshot(&fixture.baseline);
    let inventory = Arc::new(fixture.capture());
    let preview = inventory.preview();
    assert!(preview.instances[0].ordinary_import_available);
    assert!(!preview.cutover_available);
    assert!(inventory.obligations().is_empty());
    assert_eq!(
        inventory
            .record_bytes(&format!("profile/{USER_MOD_WITNESS_PATH}"))
            .unwrap(),
        fs::read(fixture.baseline.join(USER_MOD_WITNESS_PATH)).unwrap()
    );
    let (root, service) = import_service();
    let imported = service
        .import_instance(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let destination = imported_path(&service, &imported.id);
    assert_eq!(
        fs::read(destination.join("options.txt")).unwrap(),
        fs::read(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt"))
        )
        .unwrap()
    );
    let copied = snapshot(&destination);
    let library_id = service
        .directories()
        .library()
        .admit()
        .unwrap()
        .library_id();
    drop(service);
    let service = reopen_import_service(root.path(), library_id);
    let repeated = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(snapshot(&destination), copied);
    assert!(!root.path().join(USER_MOD_WITNESS_PATH).exists());
    assert!(!destination.join(USER_MOD_WITNESS_PATH).exists());
    assert!(!root.path().join("guardian").exists());
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[test]
fn guardian_user_mod_witness_import_preserves_legacy_bounds_and_empty_records() {
    for boundary in ["empty", "records", "entries", "bytes"] {
        let fixture = Fixture::new();
        let mut record = guardian_user_mod_witness_snapshot();
        match boundary {
            "empty" => record["witnesses"] = json!([]),
            "records" => record["witnesses"] = json!((0..1024).map(|index| json!({
                "instance_id": format!("{index:016x}"), "instance_created_at": "2020-01-01T00:00:00Z", "entries": []
            })).collect::<Vec<_>>()),
            "entries" => record["witnesses"][0]["entries"] = json!(vec![
                json!({"digest":"0".repeat(64), "size":0, "modified_at_ns":0}); 1024
            ]),
            _ => {}
        }
        let mut bytes = serde_json::to_vec(&record).unwrap();
        if boundary == "bytes" {
            bytes.resize(2 * 1024 * 1024, b' ');
        }
        fs::write(fixture.baseline.join(USER_MOD_WITNESS_PATH), &bytes).unwrap();
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            inventory.preview().instances[0].ordinary_import_available,
            "{boundary}"
        );
        inventory
            .prepare_instance(inventory.fingerprint(), FIRST)
            .unwrap()
            .revalidate()
            .unwrap();
        assert_eq!(
            inventory
                .record_bytes(&format!("profile/{USER_MOD_WITNESS_PATH}"))
                .unwrap(),
            bytes
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn guardian_user_mod_witness_import_refuses_malformed_original_records() {
    for invalid in [
        "schema",
        "version",
        "envelope-field",
        "record-field",
        "entry-field",
        "missing",
        "id",
        "timestamp",
        "timestamp-limit",
        "duplicate-record",
        "record-order",
        "digest",
        "digest-order",
        "size-order",
        "time-order",
        "negative",
        "overflow",
        "fraction",
        "records-limit",
        "entries-limit",
        "bytes-limit",
        "duplicate-envelope-field",
        "duplicate-record-field",
        "duplicate-entry-field",
        "malformed",
    ] {
        let fixture = Fixture::new();
        let mut record = guardian_user_mod_witness_snapshot();
        match invalid {
            "schema" => record["schema"] = json!("axial.guardian_user_mod_witnesses.v2"),
            "version" => record["schema_version"] = json!(2),
            "envelope-field" => record["effect"] = json!(true),
            "record-field" => record["witnesses"][0]["effect"] = json!(true),
            "entry-field" => record["witnesses"][0]["entries"][0]["effect"] = json!(true),
            "missing" => { record["witnesses"][0].as_object_mut().unwrap().remove("instance_created_at"); }
            "id" => record["witnesses"][0]["instance_id"] = json!("000000000000000A"),
            "timestamp" => record["witnesses"][0]["instance_created_at"] = json!("not-a-time"),
            "timestamp-limit" => record["witnesses"][0]["instance_created_at"] = json!(format!("2024-01-01T00:00:00.{}Z", "1".repeat(50))),
            "duplicate-record" => record["witnesses"][1] = record["witnesses"][0].clone(),
            "record-order" => record["witnesses"].as_array_mut().unwrap().reverse(),
            "digest" => record["witnesses"][0]["entries"][0]["digest"] = json!("A".repeat(64)),
            "digest-order" => record["witnesses"][0]["entries"][1]["digest"] = json!("0".repeat(64)),
            "size-order" => record["witnesses"][0]["entries"][1]["size"] = json!(u64::MAX - 1),
            "time-order" => record["witnesses"][0]["entries"][1]["modified_at_ns"] = json!(u64::MAX - 1),
            "negative" => record["witnesses"][0]["entries"][0]["size"] = json!(-1),
            "fraction" => record["witnesses"][0]["entries"][0]["modified_at_ns"] = json!(1.5),
            "records-limit" => record["witnesses"] = json!((0..1025).map(|index| json!({
                "instance_id": format!("{index:016x}"), "instance_created_at": "2020-01-01T00:00:00Z", "entries": []
            })).collect::<Vec<_>>()),
            "entries-limit" => record["witnesses"][0]["entries"] = json!(vec![record["witnesses"][0]["entries"][0].clone(); 1025]),
            _ => {}
        }
        let mut bytes = serde_json::to_vec(&record).unwrap();
        match invalid {
            "bytes-limit" => bytes.resize(2 * 1024 * 1024 + 1, b' '),
            "duplicate-envelope-field" | "duplicate-record-field" | "duplicate-entry-field" => {
                let field = match invalid {
                    "duplicate-envelope-field" => "\"schema_version\":1".to_owned(),
                    "duplicate-record-field" => format!("\"instance_id\":\"{FIRST}\""),
                    _ => format!("\"size\":{}", u64::MAX),
                };
                let original = String::from_utf8(bytes).unwrap();
                bytes = original
                    .replacen(&field, &format!("{field},{field}"), 1)
                    .into_bytes();
                assert_ne!(bytes, original.as_bytes());
            }
            "overflow" => {
                bytes = String::from_utf8(bytes)
                    .unwrap()
                    .replacen(&u64::MAX.to_string(), "18446744073709551616", 1)
                    .into_bytes()
            }
            "malformed" => bytes = b"{".to_vec(),
            _ => {}
        }
        fs::write(fixture.baseline.join(USER_MOD_WITNESS_PATH), &bytes).unwrap();
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{invalid}"
        );
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(
            inventory
                .record_bytes(&format!("profile/{USER_MOD_WITNESS_PATH}"))
                .unwrap(),
            bytes
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn guardian_user_mod_witness_import_keeps_source_fences() {
    for boundary in ["changed", "renamed"] {
        let fixture = Fixture::new();
        let mut record = guardian_user_mod_witness_snapshot();
        fixture.write(USER_MOD_WITNESS_PATH, &record);
        let inventory = Arc::new(fixture.capture());
        let prepared = inventory
            .prepare_instance(inventory.fingerprint(), FIRST)
            .unwrap();
        if boundary == "changed" {
            record["witnesses"][0]["entries"] = json!([]);
            fixture.write(USER_MOD_WITNESS_PATH, &record);
        } else {
            fs::rename(
                fixture.baseline.join(USER_MOD_WITNESS_PATH),
                fixture.baseline.join("old-witness.json"),
            )
            .unwrap();
        }
        let before = snapshot(&fixture.baseline);
        assert!(matches!(
            inventory.revalidate(),
            Err(ImportError::SourceChanged)
        ));
        let (_root, service) = import_service();
        assert!(service.import_instance(prepared).is_err());
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.pending().unwrap().is_empty());
        if boundary == "changed" {
            let refreshed = fixture.capture();
            assert_ne!(refreshed.fingerprint(), inventory.fingerprint());
            assert!(refreshed.preview().instances[0].ordinary_import_available);
        }
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn guardian_user_mod_witness_import_does_not_waive_unknown_topology_or_effects() {
    for boundary in [
        "case",
        "nested",
        "directory",
        "sibling",
        "deletion",
        "journal",
        "hardlink",
    ] {
        let fixture = Fixture::new();
        let record = guardian_user_mod_witness_snapshot();
        fixture.write(USER_MOD_WITNESS_PATH, &record);
        match boundary {
            "case" => {
                fs::remove_file(fixture.baseline.join(USER_MOD_WITNESS_PATH)).unwrap();
                fixture.write("Guardian-user-mod-witnesses.json", &record);
            }
            "nested" => {
                fs::remove_file(fixture.baseline.join(USER_MOD_WITNESS_PATH)).unwrap();
                fixture.write(&format!("state/{USER_MOD_WITNESS_PATH}"), &record);
            }
            "directory" => {
                fs::remove_file(fixture.baseline.join(USER_MOD_WITNESS_PATH)).unwrap();
                fs::create_dir(fixture.baseline.join(USER_MOD_WITNESS_PATH)).unwrap();
            }
            "sibling" => fixture.write("guardian-user-mod-witnesses.json.next", &record),
            "deletion" => {
                let mut registry = fixture.record("instances.json");
                registry["pending_deletions"] =
                    json!([{"instance_id": FIRST, "delete_files": true}]);
                fixture.write("instances.json", &registry);
            }
            "journal" => {
                let mut journal = terminal_performance_journal();
                journal["entries"][0]["status"] = json!("Running");
                journal["entries"][0]["outcome"] = Value::Null;
                fixture.write("state/operation-journals.json", &journal);
            }
            "hardlink" => fs::hard_link(
                fixture.baseline.join(USER_MOD_WITNESS_PATH),
                fixture.root.path().join("external-witness.json"),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.baseline);
        let captured = Inventory::capture(&fixture.source, &BTreeMap::new());
        if boundary != "hardlink" {
            assert!(captured.is_ok(), "{boundary}");
        }
        if let Ok(inventory) = captured {
            let inventory = Arc::new(inventory);
            if boundary == "journal" {
                assert!(
                    inventory
                        .preview()
                        .blockers
                        .contains(&ImportBlocker::UnsettledOperation)
                );
            }
            assert!(
                !inventory.preview().instances[0].ordinary_import_available,
                "{boundary}"
            );
            assert!(
                inventory
                    .prepare_instance(inventory.fingerprint(), FIRST)
                    .is_err(),
                "{boundary}"
            );
        }
        assert_eq!(snapshot(&fixture.baseline), before);
    }
    #[cfg(unix)]
    {
        let fixture = Fixture::new();
        let external = fixture.root.path().join("external-witness.json");
        fs::write(
            &external,
            serde_json::to_vec(&guardian_user_mod_witness_snapshot()).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&external, fixture.baseline.join(USER_MOD_WITNESS_PATH))
            .unwrap();
        let before = snapshot(fixture.root.path());
        let inventory = Arc::new(fixture.capture());
        assert!(
            inventory
                .preview()
                .blockers
                .contains(&ImportBlocker::UnsafeFile)
        );
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err()
        );
        assert_eq!(snapshot(fixture.root.path()), before);
    }
}

#[test]
fn invalid_offline_identity_is_not_replaced_with_new_identity() {
    let fixture = Fixture::new();
    let mut accounts = fixture.record("accounts.json");
    accounts["accounts"][0]["offline_uuid"] = json!("11111111111111111111111111111111");
    fixture.write("accounts.json", &accounts);
    let inventory = fixture.capture();
    assert!(
        inventory
            .preview()
            .blockers
            .contains(&ImportBlocker::AccountRequiresConversion)
    );
    assert_eq!(
        inventory
            .obligations()
            .iter()
            .find(|record| record.source_record.ends_with("#/accounts/0"))
            .unwrap()
            .original,
        Some(accounts["accounts"][0].clone())
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_preserved_and_never_followed() {
    let fixture = Fixture::new();
    let outside = fixture.root.path().join("canary");
    fs::write(&outside, b"do not read or change through source link").unwrap();
    std::os::unix::fs::symlink(
        &outside,
        fixture
            .baseline
            .join(format!("instances/{FIRST}/linked-file")),
    )
    .unwrap();
    let before = snapshot(&fixture.baseline);
    let inventory = fixture.capture();
    assert!(
        inventory.preview().instances[0]
            .blockers
            .contains(&ImportBlocker::UnsafeFile)
    );
    assert_eq!(before, snapshot(&fixture.baseline));
    assert!(matches!(
        inventory.record_bytes(&format!("instances/{FIRST}/linked-file")),
        Err(ImportError::InvalidData)
    ));
}

#[test]
fn serving_a_preview_rechecks_source_and_forgetting_does_not_mutate_it() {
    let fixture = Fixture::new();
    let previews = ImportPreviews::new();
    assert!(matches!(previews.current(), Err(ImportError::NoSource)));
    let first = previews.admit(fixture.capture()).unwrap();
    assert_eq!(previews.current().unwrap(), first);
    fixture.write("accounts.json", &json!({ "changed": true }));
    assert!(matches!(
        previews.current(),
        Err(ImportError::SourceChanged)
    ));
    let before = snapshot(&fixture.baseline);
    previews.forget().unwrap();
    assert!(matches!(previews.current(), Err(ImportError::NoSource)));
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[test]
fn display_snapshot_admission_does_not_grant_changed_source_publication_authority() {
    let fixture = Fixture::new();
    let inventory = fixture.capture();
    fixture.write("accounts.json", &json!({ "changed": true }));
    let before = snapshot(&fixture.baseline);
    let previews = ImportPreviews::new();
    let preview = previews.admit(inventory).unwrap();
    assert!(matches!(
        previews.current(),
        Err(ImportError::SourceChanged)
    ));
    assert!(matches!(
        previews.prepare_instance(&preview.fingerprint, FIRST),
        Err(ImportError::SourceChanged)
    ));
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[test]
fn instance_import_freezes_source_launch_values_and_keeps_profile_cutover_blocked() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.baseline);
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    let prepared = previews
        .prepare_instance(&preview.fingerprint, FIRST)
        .unwrap();
    let instance = prepared.instance();
    assert_eq!(prepared.legacy_id(), FIRST);
    assert_ne!(instance.id.as_str(), FIRST);
    assert_eq!(instance.name, "Vanilla Fixture");
    assert_eq!(instance.version_id, "1.20.1");
    assert_eq!(instance.settings.max_memory_mb, 2048);
    assert_eq!(instance.settings.min_memory_mb, 512);
    assert_eq!(instance.settings.window_width, 1280);
    assert_eq!(instance.settings.window_height, 720);
    assert_eq!(instance.settings.performance_mode, "vanilla");
    assert_eq!(
        instance.settings.extra_jvm_args,
        "-Dfixture.marker=synthetic"
    );
    prepared
        .validate_destination(&crate::settings::ConfigView::default())
        .unwrap();
    let mut different_defaults = crate::settings::ConfigView::default();
    different_defaults.jvm_preset = crate::settings::ConfigJvmPreset::Smooth;
    assert!(prepared.validate_destination(&different_defaults).is_err());
    let again = Arc::new(fixture.capture());
    let repeated = again
        .prepare_instance(&again.preview().fingerprint, FIRST)
        .unwrap();
    assert_eq!(prepared.source_id(), repeated.source_id());
    assert_eq!(prepared.fingerprint(), repeated.fingerprint());
    assert!(!previews.current().unwrap().cutover_available);
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[test]
fn manual_modded_import_preserves_canonical_selection_and_effective_settings() {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    for component in [
        LoaderComponentId::Fabric,
        LoaderComponentId::Quilt,
        LoaderComponentId::Forge,
        LoaderComponentId::NeoForge,
    ] {
        for (global, local, expected, declare_loader, declare_game) in [
            ("custom", "", "custom", false, false),
            ("managed", "custom", "custom", false, true),
            ("managed", "vanilla", "vanilla", true, false),
            ("vanilla", "", "vanilla", true, true),
        ] {
            let fixture = Fixture::new();
            // A valid encoded selection can exceed the old Vanilla-only limit.
            let game = "1.20.1+local";
            let version = installed_version_id_for(component, game, &"v".repeat(90)).unwrap();
            assert!(version.len() > 128);
            let mut registry = fixture.record("instances.json");
            registry["instances"][0]["version_id"] = json!(version);
            registry["instances"][0]["loader_key"] = json!(if declare_loader {
                component.short_key()
            } else {
                ""
            });
            registry["instances"][0]["minecraft_version"] =
                json!(if declare_game { game } else { "" });
            registry["instances"][0]["performance_mode"] = json!(local);
            fixture.write("instances.json", &registry);
            let mut config = fixture.record("config.json");
            config["performance_mode"] = json!(global);
            fixture.write("config.json", &config);
            fs::write(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/mods/manual.jar")),
                b"user-managed mod",
            )
            .unwrap();
            let before = snapshot(&fixture.baseline);
            let inventory = Arc::new(fixture.capture());
            let preview = inventory.preview();
            assert!(preview.instances[0].ordinary_import_available);
            assert_eq!(preview.instances[0].loader_key, component.short_key());
            assert!(!preview.cutover_available);
            let prepared = inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap();
            let instance = prepared.instance();
            assert_eq!(instance.version_id, version);
            assert_eq!(instance.minecraft_version, game);
            assert_eq!(instance.loader_key, component.short_key());
            assert_eq!(instance.settings.performance_mode, expected);
            assert!(!instance.settings.auto_optimize);
            prepared
                .validate_destination(&crate::settings::ConfigView::default())
                .unwrap();
            let mut incompatible = crate::settings::ConfigView::default();
            incompatible.jvm_preset = crate::settings::ConfigJvmPreset::Smooth;
            assert!(prepared.validate_destination(&incompatible).is_err());
            assert_eq!(snapshot(&fixture.baseline), before);
        }
    }
}

#[test]
fn manual_modded_import_rejects_malformed_or_contradictory_selection() {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    let canonical =
        installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.16.9").unwrap();
    for (version, loader, game) in [
        ("1.20.1".to_owned(), "fabric", "1.20.1"),
        ("fabric-loader-0.16.9-1.20.1".into(), "fabric", "1.20.1"),
        ("loader-v2-invalid".into(), "fabric", "1.20.1"),
        ("loader-v2-invalid".into(), "", "loader-v2-invalid"),
        ("loader-v2-invalid".into(), "vanilla", "loader-v2-invalid"),
        (format!("{canonical}="), "fabric", "1.20.1"),
        (canonical.clone(), "quilt", "1.20.1"),
        (canonical.clone(), "Fabric", "1.20.1"),
        (canonical.clone(), "vanilla", "1.20.1"),
        (canonical.clone(), " ", "1.20.1"),
        (canonical.clone(), "fabric", " "),
        (canonical.clone(), "fabric", "1.20.2"),
        ("1.20.1".into(), "", ""),
        ("fabric-loader-0.16.9-1.20.1".into(), "", ""),
        (format!("../{canonical}"), "fabric", "1.20.1"),
    ]
    .into_iter()
    .chain(
        [
            "../1.20.1",
            "1:20:1",
            "1 20 1",
            ".",
            "..",
            "1.20.1é",
            "NUL",
            "1.20.",
        ]
        .into_iter()
        .flat_map(|game| {
            let version =
                installed_version_id_for(LoaderComponentId::Fabric, game, "0.16.9").unwrap();
            [(version.clone(), "fabric", game), (version, "", "")]
        }),
    ) {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["version_id"] = json!(version);
        registry["instances"][0]["loader_key"] = json!(loader);
        registry["instances"][0]["minecraft_version"] = json!(game);
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(
            !preview.instances[0].ordinary_import_available,
            "{version}/{loader}/{game}"
        );
        assert_eq!(preview.instances[0].loader_key, loader);
        assert!(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .is_err()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn vanilla_import_preserves_representable_legacy_coordinates_without_truncation() {
    for (version, available) in [
        ("1.20.1+local".to_owned(), true),
        ("v".repeat(128), true),
        ("v".repeat(129), true),
        ("v".repeat(250), true),
        ("v".repeat(251), false),
        ("v".repeat(256), false),
        ("v".repeat(257), false),
        ("NUL".into(), false),
        ("1.20.".into(), false),
        ("../1.20.1".into(), false),
    ] {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["version_id"] = json!(version);
        registry["instances"][0]["minecraft_version"] = json!(version);
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert_eq!(
            preview.instances[0].ordinary_import_available, available,
            "{version}"
        );
        let prepared = inventory.prepare_instance(&preview.fingerprint, FIRST);
        if available {
            let prepared = prepared.unwrap();
            assert_eq!(prepared.instance().version_id, version);
            assert_eq!(prepared.instance().minecraft_version, version);
            assert_eq!(prepared.instance().loader_key, "vanilla");
            assert_eq!(prepared.instance().settings.performance_mode, "vanilla");
            prepared
                .validate_destination(&crate::settings::ConfigView::default())
                .unwrap();
            let (_root, service) = import_service();
            let imported = service
                .import_instance(prepared)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(imported.version_id, version);
            assert_eq!(imported.minecraft_version, version);
            let stored = service.registry().get_live(&imported.id).unwrap().instance;
            assert_eq!(stored.version_id, version);
            assert_eq!(stored.minecraft_version, version);
            assert_eq!(stored.loader_key, "vanilla");
            assert!(service.pending().unwrap().is_empty());
        } else {
            assert!(prepared.is_err(), "{version}");
        }
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn provider_resolved_vanilla_import_preserves_evidence_and_copy_retry_after_forget() {
    use crate::{
        catalog::tests::{fixture_catalog, manifest},
        tasks::CancellationToken,
    };

    let fixture = Fixture::new();
    fixture.two_instances();
    let mut registry = fixture.record("instances.json");
    for (index, loader) in ["", "vanilla"].into_iter().enumerate() {
        registry["instances"][index]["loader_key"] = json!(loader);
        registry["instances"][index]["minecraft_version"] = json!("");
    }
    fixture.write("instances.json", &registry);
    let before = snapshot(&fixture.baseline);
    let captured = fixture.capture();
    let original_preview = captured.preview();
    assert!(
        original_preview
            .instances
            .iter()
            .all(|row| !row.ordinary_import_available)
    );
    let (catalog, server) = fixture_catalog(manifest(&[("1.20.1", "release")]));
    let cancel = CancellationToken::new();
    let inventory = captured.resolve_versions(&catalog, &cancel).await.unwrap();
    server.join().unwrap();
    let preview = inventory.preview();
    assert!(
        preview
            .instances
            .iter()
            .all(|row| row.ordinary_import_available && row.loader_key == "vanilla")
    );
    let mut unchanged = preview.clone();
    unchanged.instances = original_preview.instances.clone();
    assert_eq!(unchanged, original_preview);
    assert_eq!(inventory.instances()[0].original, registry["instances"][0]);
    assert_eq!(inventory.instances()[1].original, registry["instances"][1]);
    assert_eq!(
        inventory.record_bytes("profile/instances.json").unwrap(),
        fs::read(fixture.baseline.join("instances.json")).unwrap()
    );

    let previews = ImportPreviews::new();
    previews.admit(inventory).unwrap();
    let prepared = previews
        .prepare_instance(&preview.fingerprint, FIRST)
        .unwrap();
    assert_eq!(prepared.instance().version_id, "1.20.1");
    assert_eq!(prepared.instance().minecraft_version, "1.20.1");
    assert_eq!(prepared.instance().loader_key, "vanilla");
    assert_eq!(prepared.instance().settings.performance_mode, "vanilla");
    assert_eq!(
        prepared.instance().settings.extra_jvm_args,
        "-Dfixture.marker=synthetic"
    );
    prepared
        .validate_destination(&crate::settings::ConfigView::default())
        .unwrap();
    let mut incompatible = crate::settings::ConfigView::default();
    incompatible.jvm_preset = crate::settings::ConfigJvmPreset::Smooth;
    assert!(prepared.validate_destination(&incompatible).is_err());
    let (_root, service) = import_service();
    let work = service.import_instance(prepared).unwrap();
    previews.forget().unwrap();
    cancel.cancel();
    let imported = work.join().await.unwrap().unwrap();
    let path = imported_path(&service, &imported.id);
    assert_eq!(
        fs::read(path.join("options.txt")).unwrap(),
        fs::read(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt"))
        )
        .unwrap()
    );
    fs::write(path.join("options.txt"), b"retained destination edit").unwrap();
    let destination_before = snapshot(&path);

    let (catalog, server) = fixture_catalog(manifest(&[("1.20.1", "release")]));
    let readmitted = fixture
        .capture()
        .resolve_versions(&catalog, &CancellationToken::new())
        .await
        .unwrap();
    server.join().unwrap();
    assert_eq!(readmitted.preview(), preview);
    previews.admit(readmitted).unwrap();
    let repeated = service
        .import_instance(
            previews
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(repeated.version_id, "1.20.1");
    assert_eq!(repeated.minecraft_version, "1.20.1");
    assert_eq!(service.registry().list().unwrap().len(), 1);
    assert_eq!(snapshot(&path), destination_before);
    assert_eq!(snapshot(&fixture.baseline), before);
    assert!(!preview.cutover_available);
}

#[tokio::test]
async fn provider_resolved_vanilla_import_keeps_unrelated_slices_on_missing_or_failed_evidence() {
    use crate::{
        catalog::tests::{fixture_catalog_status, manifest},
        tasks::CancellationToken,
    };

    for (body, status) in [
        (manifest(&[("1.20.2", "release")]), 200),
        (b"malformed provider response".to_vec(), 200),
        (Vec::new(), 404),
    ] {
        let fixture = Fixture::new();
        fixture.two_instances();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["loader_key"] = json!("");
        registry["instances"][0]["minecraft_version"] = json!("");
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let captured = fixture.capture();
        let original_preview = captured.preview();
        assert!(!original_preview.instances[0].ordinary_import_available);
        assert!(original_preview.instances[1].ordinary_import_available);
        assert!(original_preview.metadata_import_available);
        let (catalog, server) = fixture_catalog_status(body, status);
        let inventory = Arc::new(
            captured
                .resolve_versions(&catalog, &CancellationToken::new())
                .await
                .unwrap(),
        );
        server.join().unwrap();
        assert_eq!(inventory.preview(), original_preview);
        assert!(
            inventory
                .prepare_instance(&original_preview.fingerprint, FIRST)
                .is_err()
        );
        inventory
            .prepare_instance(&original_preview.fingerprint, SECOND)
            .unwrap();
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn provider_resolved_vanilla_import_never_overrides_contradictions_or_settings_gates() {
    use crate::{
        catalog::tests::{fixture_catalog, manifest},
        tasks::CancellationToken,
    };

    for (field, value) in [
        ("loader_key", json!("fabric")),
        ("loader_key", json!("Vanilla")),
        ("loader_key", json!(" ")),
        ("loader_key", Value::Null),
        ("minecraft_version", json!("1.20.2")),
        ("minecraft_version", json!(" ")),
        ("minecraft_version", Value::Null),
        ("performance_mode", json!("future-mode")),
        ("auto_optimize", json!("true")),
        ("java_path", json!("/untrusted/java")),
    ] {
        let fixture = Fixture::new();
        fixture.two_instances();
        let mut registry = fixture.record("instances.json");
        for instance in registry["instances"].as_array_mut().unwrap() {
            instance["loader_key"] = json!("");
            instance["minecraft_version"] = json!("");
        }
        registry["instances"][1][field] = value;
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let (catalog, server) = fixture_catalog(manifest(&[("1.20.1", "release")]));
        let inventory = Arc::new(
            fixture
                .capture()
                .resolve_versions(&catalog, &CancellationToken::new())
                .await
                .unwrap(),
        );
        server.join().unwrap();
        let preview = inventory.preview();
        assert!(preview.instances[0].ordinary_import_available, "{field}");
        assert!(!preview.instances[1].ordinary_import_available, "{field}");
        assert!(
            inventory
                .prepare_instance(&preview.fingerprint, SECOND)
                .is_err()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn provider_resolved_vanilla_import_skips_network_for_declared_canonical_or_invalid_selections()
 {
    use crate::{
        catalog::tests::{fixture_catalog, manifest},
        tasks::CancellationToken,
    };
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};

    let (catalog, server) = fixture_catalog(manifest(&[("1.20.1", "release")]));
    let mut cases = vec![
        ("1.20.1".to_owned(), "", "1.20.1", true),
        ("1.20.1+local".into(), "vanilla", "1.20.1+local", true),
        ("loader-v2-invalid".into(), "", "", false),
        ("../1.20.1".into(), "", "", false),
        ("1:20:1".into(), "", "", false),
        ("1 20 1".into(), "", "", false),
        ("v".repeat(251), "", "", false),
        ("1.20.1".into(), "fabric", "", false),
        ("1.20.1".into(), "", " ", false),
    ];
    for component in [
        LoaderComponentId::Fabric,
        LoaderComponentId::Quilt,
        LoaderComponentId::Forge,
        LoaderComponentId::NeoForge,
    ] {
        cases.push((
            installed_version_id_for(component, "1.20.1", "1.0").unwrap(),
            "",
            "",
            true,
        ));
    }
    for (version, loader, game, available) in cases {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["version_id"] = json!(version);
        registry["instances"][0]["loader_key"] = json!(loader);
        registry["instances"][0]["minecraft_version"] = json!(game);
        fixture.write("instances.json", &registry);
        let captured = fixture.capture();
        let preview = captured.preview();
        let inventory = captured
            .resolve_versions(&catalog, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(inventory.preview(), preview, "{version}/{loader}/{game}");
        assert_eq!(preview.instances[0].ordinary_import_available, available);
    }
    // The sole response must still be available: the prior selections neither
    // require nor may consume missing-declaration provider evidence.
    let fixture = Fixture::new();
    let mut registry = fixture.record("instances.json");
    registry["instances"][0]["minecraft_version"] = json!("");
    fixture.write("instances.json", &registry);
    let inventory = fixture
        .capture()
        .resolve_versions(&catalog, &CancellationToken::new())
        .await
        .unwrap();
    server.join().unwrap();
    assert!(inventory.preview().instances[0].ordinary_import_available);
}

#[tokio::test]
async fn provider_resolved_vanilla_import_checks_cancellation_and_source_before_provider_io() {
    use crate::{
        catalog::tests::{fixture_catalog, manifest},
        tasks::CancellationToken,
    };

    let fixture = Fixture::new();
    let mut registry = fixture.record("instances.json");
    registry["instances"][0]["minecraft_version"] = json!("");
    fixture.write("instances.json", &registry);
    let (catalog, server) = fixture_catalog(manifest(&[("1.20.1", "release")]));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        fixture.capture().resolve_versions(&catalog, &cancel).await,
        Err(ImportError::Cancelled)
    ));
    let captured = fixture.capture();
    let source_file = fixture
        .baseline
        .join(format!("instances/{FIRST}/options.txt"));
    fs::write(source_file, b"changed before provider lookup").unwrap();
    assert!(matches!(
        captured
            .resolve_versions(&catalog, &CancellationToken::new())
            .await,
        Err(ImportError::SourceChanged)
    ));
    // Neither failed preflight consumed the provider response.
    let before = snapshot(&fixture.baseline);
    let inventory = fixture
        .capture()
        .resolve_versions(&catalog, &CancellationToken::new())
        .await
        .unwrap();
    server.join().unwrap();
    assert!(inventory.preview().instances[0].ordinary_import_available);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn provider_resolved_vanilla_import_rejects_source_drift_or_capture_cancel_during_lookup() {
    use crate::{
        catalog::tests::{fixture_catalog_on_request, manifest},
        tasks::CancellationToken,
    };

    for cancel_capture in [false, true] {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["minecraft_version"] = json!("");
        fixture.write("instances.json", &registry);
        let mut expected = snapshot(&fixture.baseline);
        let cancelled = Arc::new(AtomicBool::new(false));
        let inventory = Inventory::capture_controlled(
            &fixture.source,
            &BTreeMap::new(),
            CaptureLimits::default(),
            cancelled.clone(),
        )
        .unwrap();
        let relative = PathBuf::from(format!("instances/{FIRST}/options.txt"));
        let source_file = fixture.baseline.join(&relative);
        let changed_bytes = b"source changed during provider lookup";
        if !cancel_capture {
            expected.get_mut(&relative).unwrap().0 = changed_bytes.to_vec();
        }
        let (catalog, server) =
            fixture_catalog_on_request(manifest(&[("1.20.1", "release")]), move || {
                if cancel_capture {
                    cancelled.store(true, std::sync::atomic::Ordering::Release);
                } else {
                    fs::write(source_file, changed_bytes).unwrap();
                }
            });
        let result = inventory
            .resolve_versions(&catalog, &CancellationToken::new())
            .await;
        server.join().unwrap();
        if cancel_capture {
            assert!(matches!(result, Err(ImportError::Cancelled)));
        } else {
            assert!(matches!(result, Err(ImportError::SourceChanged)));
        }
        assert_eq!(snapshot(&fixture.baseline), expected);
    }
}

#[test]
fn manual_modded_import_does_not_waive_invalid_or_untrusted_settings() {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    for (global, local, auto_optimize, java_path) in [
        ("future-mode", "", false, ""),
        ("custom", "future-mode", false, ""),
        ("custom", "custom", false, "/not-admitted/java"),
    ] {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["version_id"] =
            json!(installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.16.9").unwrap());
        registry["instances"][0]["loader_key"] = json!("fabric");
        registry["instances"][0]["performance_mode"] = json!(local);
        registry["instances"][0]["auto_optimize"] = json!(auto_optimize);
        registry["instances"][0]["java_path"] = json!(java_path);
        fixture.write("instances.json", &registry);
        let mut config = fixture.record("config.json");
        config["performance_mode"] = json!(global);
        fixture.write("config.json", &config);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(!preview.instances[0].ordinary_import_available);
        assert!(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .is_err()
        );
    }
}

#[test]
fn instance_import_preserves_effective_managed_java_ids_against_destination_defaults() {
    use crate::settings::ConfigView;
    for (global, local, expected) in [
        ("java-runtime-gamma", "", "java-runtime-gamma"),
        (
            "java-runtime-alpha",
            "java-runtime-delta",
            "java-runtime-delta",
        ),
        ("", " jre-legacy ", "jre-legacy"),
        (" java-runtime-beta ", " ", "java-runtime-beta"),
    ] {
        let fixture = Fixture::new();
        let mut config = fixture.record("config.json");
        config["java_path_override"] = json!(global);
        fixture.write("config.json", &config);
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["java_path"] = json!(local);
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            inventory.preview().instances[0].ordinary_import_available,
            "global={global:?}, local={local:?}"
        );
        let prepared = inventory
            .prepare_instance(inventory.fingerprint(), FIRST)
            .unwrap();
        assert_eq!(prepared.instance().settings.java_path, expected);
        for destination_java in ["", "java-runtime-epsilon", "/destination/custom/java"] {
            let destination = ConfigView {
                java_path_override: destination_java.into(),
                ..ConfigView::default()
            };
            prepared.validate_destination(&destination).unwrap();
            assert_eq!(
                prepared
                    .instance()
                    .settings
                    .effective(&destination)
                    .unwrap()
                    .java_path,
                expected
            );
        }
        assert_eq!(inventory.instances()[0].original["java_path"], local);
        assert_eq!(snapshot(&fixture.baseline), before);
    }
    // An empty source means Automatic, not permission to inherit another
    // destination's explicit Java choice.
    let fixture = Fixture::new();
    let prepared = prepare_first(&fixture);
    assert!(
        prepared
            .validate_destination(&ConfigView {
                java_path_override: "java-runtime-delta".into(),
                ..ConfigView::default()
            })
            .is_err()
    );
}

#[tokio::test]
async fn instance_import_managed_java_selection_survives_copy_reopen_and_missing_runtime() {
    use crate::{
        runtime::{discovery::RuntimeDiscovery, model::JavaDiscoveryError},
        settings::SettingsStore,
        storage::MetadataStore,
        tasks::{CancellationToken, TaskOwner},
    };
    let fixture = Fixture::new();
    let mut config = fixture.record("config.json");
    config["java_path_override"] = json!("java-runtime-gamma");
    fixture.write("config.json", &config);
    let before = snapshot(&fixture.baseline);
    let (root, service) = import_service();
    let settings = SettingsStore::new(Arc::new(
        MetadataStore::open(root.path().join("metadata.sqlite")).unwrap(),
    ))
    .unwrap();
    let destination = settings.update(serde_json::from_value(json!({"expected_revision":settings.current().unwrap().revision,"java_path_override":"java-runtime-delta"})).unwrap()).unwrap();
    let imported = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert!(
        imported.settings.java_path.is_empty(),
        "public responses retain runtime-override redaction"
    );
    let stored = service.registry().get_live(&imported.id).unwrap().instance;
    assert_eq!(stored.settings.java_path, "java-runtime-gamma");
    assert_eq!(
        stored.settings.effective(&destination).unwrap().java_path,
        "java-runtime-gamma"
    );
    let runtime = RuntimeDiscovery::new(
        axial_minecraft::ManagedRuntimeCache::isolated_for_test().unwrap(),
        TaskOwner::new(2).unwrap(),
    );
    assert!(
        matches!(
            runtime
                .select(
                    &axial_minecraft::JavaVersion {
                        component: "java-runtime-gamma".into(),
                        major_version: 17
                    },
                    &stored.settings.java_path,
                    &CancellationToken::new()
                )
                .await,
            Err(JavaDiscoveryError::Missing)
        ),
        "selection metadata is not an installed executable receipt"
    );
    let library_id = service
        .directories()
        .library()
        .admit()
        .unwrap()
        .library_id();
    drop(service);
    let reopened = reopen_import_service(root.path(), library_id);
    let stored = reopened.registry().get_live(&imported.id).unwrap().instance;
    assert_eq!(stored.settings.java_path, "java-runtime-gamma");
    let repeated = reopened
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert!(repeated.settings.java_path.is_empty());
    assert_eq!(
        reopened
            .registry()
            .get_live(&repeated.id)
            .unwrap()
            .instance
            .settings
            .java_path,
        "java-runtime-gamma"
    );
    assert_eq!(reopened.registry().list().unwrap().len(), 1);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[test]
fn instance_import_java_selection_keeps_unknown_ids_and_predecessor_paths_refused() {
    for value in [
        "java-runtime-future",
        "Java-runtime-gamma",
        "../java-runtime-gamma",
        "/predecessor/runtime/bin/java",
        "C:\\Predecessor\\runtime\\bin\\java.exe",
    ] {
        for global in [false, true] {
            let fixture = Fixture::new();
            if global {
                let mut config = fixture.record("config.json");
                config["java_path_override"] = json!(value);
                fixture.write("config.json", &config);
            } else {
                let mut registry = fixture.record("instances.json");
                registry["instances"][0]["java_path"] = json!(value);
                fixture.write("instances.json", &registry);
            }
            let before = snapshot(&fixture.baseline);
            let inventory = Arc::new(fixture.capture());
            assert!(!inventory.preview().instances[0].ordinary_import_available);
            assert!(
                inventory
                    .prepare_instance(inventory.fingerprint(), FIRST)
                    .is_err()
            );
            assert_eq!(snapshot(&fixture.baseline), before);
        }
    }
}

#[test]
fn manual_modded_import_keeps_retained_records_and_private_directories_blocking() {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    for blocker in [
        ImportBlocker::ManagedStateRequiresConversion,
        ImportBlocker::ContentProvenanceRequiresConversion,
        ImportBlocker::UnsettledOperation,
    ] {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0]["version_id"] =
            json!(installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.16.9").unwrap());
        registry["instances"][0]["loader_key"] = json!("fabric");
        registry["instances"][0]["performance_mode"] = json!("custom");
        fixture.write("instances.json", &registry);
        match blocker {
            ImportBlocker::ManagedStateRequiresConversion => {
                fs::create_dir_all(fixture.baseline.join(format!(
                    "instances/{FIRST}/mods/.axial-performance/unknown-lane"
                )))
                .unwrap();
            }
            ImportBlocker::ContentProvenanceRequiresConversion => fixture.write(
                &format!("instances/{FIRST}/axial.content.json"),
                &json!({"schema_version":4,"entries":[]}),
            ),
            ImportBlocker::UnsettledOperation => fixture.write(
                "state/operation-journals.json",
                &json!({"schema":"axial.state.operation_journals.v10","next_sequence":2,
                    "entries":[{"status":"Succeeded","sequence":1,
                        "targets":[{"kind":"Instance","id":FIRST}],
                        "outcome":"Succeeded"}]}),
            ),
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(preview.instances[0].blockers.contains(&blocker));
        assert!(!preview.instances[0].ordinary_import_available);
        assert!(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .is_err()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn instance_import_refuses_unsupported_semantics_instead_of_dropping_them() {
    for field in [
        "unknown_retained_preference",
        "performance_mode",
        "loader_key",
        "java_path",
    ] {
        let fixture = Fixture::new();
        let mut registry = fixture.record("instances.json");
        registry["instances"][0][field] = match field {
            "performance_mode" => json!("future-mode"),
            "loader_key" => json!("fabric"),
            "java_path" => json!("/not-admitted/java"),
            _ => json!("must survive"),
        };
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            inventory
                .prepare_instance(&inventory.preview().fingerprint, FIRST)
                .is_err(),
            "{field}"
        );
        assert_eq!(before, snapshot(&fixture.baseline));
    }
    let fixture = Fixture::new();
    fixture.write(
        &format!("instances/{FIRST}/mods/axial.content.json"),
        &json!({"retained": true}),
    );
    let before = snapshot(&fixture.baseline);
    let inventory = Arc::new(fixture.capture());
    assert!(
        inventory
            .prepare_instance(&inventory.preview().fingerprint, FIRST)
            .is_err()
    );
    assert_eq!(before, snapshot(&fixture.baseline));

    for reserved in [
        ".axial-lock.json",
        ".AXIAL-PERFORMANCE",
        "AXIAL.CONTENT.JSON",
    ] {
        let fixture = Fixture::new();
        fs::create_dir(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/{reserved}")),
        )
        .unwrap();
        let inventory = Arc::new(fixture.capture());
        assert!(
            inventory
                .prepare_instance(&inventory.preview().fingerprint, FIRST)
                .is_err(),
            "reserved directory {reserved}"
        );
    }
}

#[test]
fn prepared_import_rejects_stale_source_and_cancelled_capture() {
    let fixture = Fixture::new();
    let inventory = Arc::new(fixture.capture());
    let fingerprint = inventory.preview().fingerprint;
    assert!(matches!(
        inventory.prepare_instance("another-preview", FIRST),
        Err(ImportError::SourceChanged)
    ));
    let prepared = inventory.prepare_instance(&fingerprint, FIRST).unwrap();
    fs::write(
        fixture
            .baseline
            .join(format!("instances/{FIRST}/options.txt")),
        b"user changed source",
    )
    .unwrap();
    let changed = snapshot(&fixture.baseline);
    assert!(matches!(
        prepared.revalidate(),
        Err(ImportError::SourceChanged)
    ));
    assert!(matches!(
        inventory.prepare_instance(&fingerprint, FIRST),
        Err(ImportError::SourceChanged)
    ));
    assert_eq!(changed, snapshot(&fixture.baseline));

    let fixture = Fixture::new();
    let cancelled = Arc::new(AtomicBool::new(false));
    let inventory = Arc::new(
        Inventory::capture_controlled(
            &fixture.source,
            &BTreeMap::new(),
            CaptureLimits::default(),
            cancelled.clone(),
        )
        .unwrap(),
    );
    let before = snapshot(&fixture.baseline);
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    assert!(matches!(
        inventory.prepare_instance(&inventory.preview().fingerprint, FIRST),
        Err(ImportError::Cancelled)
    ));
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[test]
fn staged_copy_must_preserve_every_file_and_empty_directory_without_adopting_extras() {
    use crate::{
        files::ScopedDirectory,
        library::{LibraryLifecycle, LibraryOpenOutcome},
        tasks::CancellationToken,
    };
    let fixture = Fixture::new();
    let original = fixture.baseline.join("instances").join(FIRST);
    fs::create_dir(original.join("screenshots")).unwrap();
    fs::create_dir(original.join("logs")).unwrap();
    fs::create_dir(original.join("empty-user-folder")).unwrap();
    fs::write(original.join("screenshots/capture.png"), b"user screenshot").unwrap();
    fs::write(original.join("logs/latest.log"), b"user log").unwrap();
    fs::write(
        original.join("unfamiliar.user-data"),
        b"unknown ordinary user file",
    )
    .unwrap();
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap();
    let before = snapshot(&fixture.baseline);
    let destination =
        tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let copied = destination.path().join("copied");
    fs::create_dir(&copied).unwrap();
    copy_fixture(&original, &copied);
    let library = match LibraryLifecycle::open(destination.path()) {
        LibraryOpenOutcome::Ready(library) => library,
        other => panic!("isolated destination: {other:?}"),
    };
    let pin = library.admit().unwrap();
    let directory = pin
        .directory()
        .unwrap()
        .open_directory(&axial_fs::LeafName::new("copied").unwrap())
        .unwrap();
    let staged = ScopedDirectory::from_admitted(directory, pin).unwrap();
    let cancel = CancellationToken::new();
    prepared.verify_staged(&staged, &cancel).unwrap();
    fs::write(copied.join("foreign.keep"), b"do not delete").unwrap();
    assert!(prepared.verify_staged(&staged, &cancel).is_err());
    assert_eq!(
        fs::read(copied.join("foreign.keep")).unwrap(),
        b"do not delete"
    );
    fs::remove_file(copied.join("foreign.keep")).unwrap();
    fs::write(
        copied.join("unfamiliar.user-data"),
        b"modified staged user file",
    )
    .unwrap();
    assert!(prepared.verify_staged(&staged, &cancel).is_err());
    fs::write(
        copied.join("unfamiliar.user-data"),
        b"unknown ordinary user file",
    )
    .unwrap();
    fs::remove_dir(copied.join("empty-user-folder")).unwrap();
    assert!(prepared.verify_staged(&staged, &cancel).is_err());
    fs::create_dir(copied.join("empty-user-folder")).unwrap();
    prepared.verify_staged(&staged, &cancel).unwrap();
    cancel.cancel();
    let staged_before = snapshot(&copied);
    assert!(matches!(
        prepared.verify_staged(&staged, &cancel),
        Err(ImportError::Cancelled)
    ));
    assert_eq!(staged_before, snapshot(&copied));
    assert_eq!(before, snapshot(&fixture.baseline));
}

fn import_service() -> (tempfile::TempDir, crate::instances::create::InstanceService) {
    let (root, service) = crate::instances::create::tests::fixture();
    service
        .registry()
        .storage()
        .migrate(&[crate::performance::rules::MIGRATION])
        .unwrap();
    (root, service)
}

#[test]
fn import_requires_disjoint_destination_even_across_independent_admissions() {
    let fixture = Fixture::new();
    let destination_owner =
        tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let owner = match RootSession::acquire(destination_owner.path()) {
        RootSessionAcquireOutcome::Acquired(owner) => owner,
        other => panic!("independent destination owner: {other:?}"),
    };
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap();
    let before = snapshot(&fixture.baseline);
    let instance_source = fixture.baseline.join("instances").join(FIRST);
    for overlapping in [
        fixture.baseline.as_path(),
        instance_source.as_path(),
        fixture.root.path(),
    ] {
        let admitted = owner.admit_absolute_directory(overlapping).unwrap();
        assert!(prepared.validate_destination_root(&admitted).is_err());
    }
    prepared
        .validate_destination_root(&owner.root().unwrap())
        .unwrap();
    assert_eq!(before, snapshot(&fixture.baseline));

    let external = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let external_source = ReadOnlySource::from_admitted_directory(
        fixture
            .owner
            .admit_absolute_directory(external.path())
            .unwrap(),
    );
    let inventory = Arc::new(
        Inventory::capture(
            &fixture.source,
            &BTreeMap::from([(FIRST.to_owned(), external_source)]),
        )
        .unwrap(),
    );
    let prepared = inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap();
    let admitted = owner.admit_absolute_directory(external.path()).unwrap();
    assert!(prepared.validate_destination_root(&admitted).is_err());
    assert_eq!(before, snapshot(&fixture.baseline));
}

fn imported_path(
    service: &crate::instances::create::InstanceService,
    id: &crate::instances::model::InstanceId,
) -> PathBuf {
    service
        .directories()
        .library()
        .admit()
        .unwrap()
        .read_projection()
        .unwrap()
        .join("instances")
        .join(id.as_str())
}

async fn seed_managed_source(fixture: &Fixture) -> axial_performance::CompositionState {
    use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
    let (_root, service) = import_service();
    let source = crate::instances::create::tests::create(&service, "Managed fixture").await;
    let admitted = service.directories().admit(&source.id).unwrap();
    let state = crate::performance::duplicate::seed_managed(&admitted).await;
    copy_fixture(
        &admitted.directory().read_projection().unwrap().join("mods"),
        &fixture.baseline.join(format!("instances/{FIRST}/mods")),
    );
    fs::write(
        fixture
            .baseline
            .join(format!("instances/{FIRST}/mods/user-owned.jar")),
        b"user-owned mod",
    )
    .unwrap();
    let mut registry = fixture.record("instances.json");
    let instance = &mut registry["instances"][0];
    instance["version_id"] =
        json!(installed_version_id_for(LoaderComponentId::Fabric, "1.21.4", "0.16.9").unwrap());
    instance["loader_key"] = json!("fabric");
    instance["minecraft_version"] = json!("1.21.4");
    instance["performance_mode"] = json!("");
    instance["auto_optimize"] = json!(true);
    fixture.write("instances.json", &registry);
    let mut config = fixture.record("config.json");
    config["performance_mode"] = json!("managed");
    fixture.write("config.json", &config);
    state
}

fn seed_content_source(fixture: &Fixture) -> Vec<u8> {
    use sha2::{Digest, Sha512};
    let mut entries = Vec::new();
    for (project, filename, enabled, present) in [
        ("enabled", "enabled.jar", true, true),
        ("disabled", "disabled.jar", false, true),
        ("missing", "missing.jar", true, false),
        ("edited", "edited.jar", true, true),
    ] {
        let bytes = format!("source artifact {project}");
        entries.push(json!({
            "canonical_id":format!("modrinth:{project}"), "provider":"modrinth",
            "project_id":project, "version_id":"old-version", "kind":"mod",
            "filename":filename, "sha512":hex::encode(Sha512::digest(bytes.as_bytes())),
            "size":bytes.len(), "dependencies":[{"project_id":"dependency","kind":"optional"}],
            "enabled":enabled, "installed_at":"2024-02-29T12:34:56+02:00", "title":"Retained title"
        }));
        if present {
            let leaf = if enabled {
                filename.into()
            } else {
                format!("{filename}.disabled")
            };
            fs::write(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/mods/{leaf}")),
                if project == "edited" {
                    "user modified source artifact"
                } else {
                    &bytes
                },
            )
            .unwrap();
        }
    }
    entries.push(json!({
        "canonical_id":"modrinth:pack", "provider":"modrinth", "project_id":"pack",
        "version_id":"historical-pack", "kind":"modpack", "dependencies":[],
        "enabled":true, "installed_at":"2024-02-29T12:34:56+02:00", "title":"Provenance only"
    }));
    let bytes = serde_json::to_vec_pretty(&json!({"schema_version":3,"entries":entries})).unwrap();
    fs::write(
        fixture
            .baseline
            .join(format!("instances/{FIRST}/axial.content.json")),
        &bytes,
    )
    .unwrap();
    bytes
}

fn prepare_first(fixture: &Fixture) -> PreparedInstanceImport {
    let inventory = Arc::new(fixture.capture());
    assert!(inventory.preview().instances[0].ordinary_import_available);
    assert!(!inventory.preview().cutover_available);
    inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap()
}

#[tokio::test]
async fn ordinary_profile_records_saved_skins_allow_independent_instance_publication() {
    use super::skins::tests::{install, skin};
    use crate::instances::model::InstanceResult;

    for empty in [false, true] {
        let fixture = Fixture::new();
        let records = if empty { vec![] } else { vec![skin(80)] };
        install(&fixture, &records);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(preview.skin_import_available);
        assert!(preview.instances[0].ordinary_import_available);
        assert!(
            preview
                .blockers
                .contains(&ImportBlocker::SavedSkinsRequireConversion)
        );
        assert!(!preview.cutover_available);
        let (root, service) = import_service();
        service
            .registry()
            .storage()
            .migrate(&[
                crate::accounts::directory::MIGRATION,
                crate::skins::store::MIGRATION,
                crate::skins::store::IMPORT_MIGRATION,
            ])
            .unwrap();
        let imported = service
            .import_instance(
                inventory
                    .prepare_instance(&preview.fingerprint, FIRST)
                    .unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(imported_path(&service, &imported.id).join("options.txt")).unwrap(),
            fs::read(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/options.txt"))
            )
            .unwrap()
        );
        let copied = snapshot(&imported_path(&service, &imported.id));
        let library_id = service
            .directories()
            .library()
            .admit()
            .unwrap()
            .library_id();
        drop(service);
        let service = reopen_import_service(root.path(), library_id);
        let repeated = service
            .import_instance(prepare_first(&fixture))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated.id, imported.id);
        assert_eq!(snapshot(&imported_path(&service, &imported.id)), copied);
        let counts = service.registry().storage().read(|connection| -> InstanceResult<(u64, u64, u64)> {
            Ok(connection.query_row("SELECT (SELECT COUNT(*) FROM saved_skins), (SELECT COUNT(*) FROM saved_skin_imports), (SELECT COUNT(*) FROM saved_skin_accounts)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?)
        }).unwrap();
        assert_eq!(
            counts,
            (0, 0, 0),
            "instance publication must not publish or apply skins"
        );
        assert!(!root.path().join("skins").exists());
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn ordinary_profile_records_malformed_or_unknown_skins_stay_blocked() {
    use super::skins::tests::{install, skin};
    for invalid in [
        "unknown-field",
        "unlisted",
        "missing",
        "changed-png",
        "nested",
        "schema",
        "duplicate",
    ] {
        let fixture = Fixture::new();
        let skin = skin(80);
        let png = fixture
            .baseline
            .join(format!("skins/files/{}.png", skin.0.texture_key));
        install(&fixture, &[skin]);
        let mut index = fixture.record("skins/index.json");
        match invalid {
            "unknown-field" => {
                index["future"] = json!(true);
                fixture.write("skins/index.json", &index);
            }
            "unlisted" => fs::write(
                fixture.baseline.join("skins/files/unlisted.png"),
                b"retain unknown",
            )
            .unwrap(),
            "missing" => fs::remove_file(png).unwrap(),
            "changed-png" => fs::write(png, b"different bytes").unwrap(),
            "nested" => fs::create_dir(fixture.baseline.join("skins/files/nested")).unwrap(),
            "schema" => {
                index["schema_version"] = json!(4);
                fixture.write("skins/index.json", &index);
            }
            _ => {
                let duplicate = index["skins"][0].clone();
                index["skins"].as_array_mut().unwrap().push(duplicate);
                fixture.write("skins/index.json", &index);
            }
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(!inventory.preview().skin_import_available, "{invalid}");
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{invalid}"
        );
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn ordinary_profile_records_exact_music_cache_does_not_block_instance_import() {
    use crate::music::MUSIC_FILES;
    for count in 0..=MUSIC_FILES.len() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.baseline.join("music")).unwrap();
        for (index, name) in MUSIC_FILES[..count].iter().enumerate() {
            // Legacy accepts an exact bounded file, including an empty cache.
            fs::write(
                fixture.baseline.join("music").join(name),
                if index == 0 {
                    b"".as_slice()
                } else {
                    b"cached fixed track".as_slice()
                },
            )
            .unwrap();
        }
        let mut config = fixture.record("config.json");
        config["music_enabled"] = json!(true);
        config["music_volume"] = json!(37);
        config["music_track"] = json!(1);
        fixture.write("config.json", &config);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(
            preview.instances[0].ordinary_import_available,
            "{count} cached tracks"
        );
        assert!(preview.metadata_import_available);
        assert!(
            !preview
                .blockers
                .contains(&ImportBlocker::UnknownRetainedRecord)
        );
        assert!(!preview.cutover_available);
        for name in &MUSIC_FILES[..count] {
            assert_eq!(
                inventory
                    .record_bytes(&format!("profile/music/{name}"))
                    .unwrap(),
                fs::read(fixture.baseline.join("music").join(name)).unwrap()
            );
        }
        let settings =
            crate::settings::prepare_legacy_import(&fixture.record("config.json")).unwrap();
        assert_eq!(settings.config.music_enabled, Some(true));
        assert_eq!(settings.config.music_volume, Some(37));
        assert_eq!(settings.config.music_track, 1);
        let (root, service) = import_service();
        let imported = service
            .import_instance(
                inventory
                    .prepare_instance(&preview.fingerprint, FIRST)
                    .unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(
            imported_path(&service, &imported.id)
                .join("options.txt")
                .is_file()
        );
        assert!(
            !root.path().join("music").exists(),
            "cache omission must not claim offline music migration"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn ordinary_profile_records_unknown_music_topology_stays_blocked() {
    use crate::music::{MUSIC_FILES, MUSIC_MAX_BYTES};
    for invalid in [
        "unknown",
        "alias",
        "scratch",
        "nested",
        "wrong-kind",
        "root-file",
        "root-alias",
        "oversize",
    ] {
        let fixture = Fixture::new();
        let music = fixture.baseline.join("music");
        if invalid == "root-file" {
            fs::write(&music, b"not a directory").unwrap();
        } else if invalid == "root-alias" {
            fs::create_dir(fixture.baseline.join("Music")).unwrap();
        } else {
            fs::create_dir(&music).unwrap();
            match invalid {
                "unknown" => fs::write(music.join("custom.mp3"), b"user file").unwrap(),
                "alias" => fs::write(music.join("VAPOR-HALO.MP3"), b"wrong spelling").unwrap(),
                "scratch" => {
                    fs::write(music.join(".axial-rstage-retained"), b"retained effect").unwrap()
                }
                "nested" => fs::create_dir(music.join("nested")).unwrap(),
                "wrong-kind" => fs::create_dir(music.join(MUSIC_FILES[0])).unwrap(),
                _ => fs::File::create(music.join(MUSIC_FILES[0]))
                    .unwrap()
                    .set_len(MUSIC_MAX_BYTES + 1)
                    .unwrap(),
            }
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{invalid}"
        );
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[cfg(unix)]
#[test]
fn ordinary_profile_records_skin_and_music_links_are_not_admitted() {
    use super::skins::tests::{install, skin};
    for boundary in ["skin", "music-file", "music-root", "music-hardlink"] {
        let fixture = Fixture::new();
        let external = fixture.root.path().join("external");
        fs::create_dir(&external).unwrap();
        let private = external.join("private.mp3");
        fs::write(&private, b"external canary").unwrap();
        let target = if boundary == "skin" {
            let record = skin(80);
            let target = fixture
                .baseline
                .join(format!("skins/files/{}.png", record.0.texture_key));
            install(&fixture, &[record]);
            fs::remove_file(&target).unwrap();
            target
        } else if boundary == "music-root" {
            fixture.baseline.join("music")
        } else {
            fs::create_dir(fixture.baseline.join("music")).unwrap();
            fixture
                .baseline
                .join("music")
                .join(crate::music::MUSIC_FILES[0])
        };
        if boundary == "music-hardlink" {
            fs::hard_link(&private, &target).unwrap();
        } else {
            std::os::unix::fs::symlink(
                if boundary == "music-root" {
                    &external
                } else {
                    &private
                },
                &target,
            )
            .unwrap();
        }
        let source_before = snapshot(&fixture.baseline);
        let external_before = snapshot(&external);
        if let Ok(inventory) = Inventory::capture(&fixture.source, &BTreeMap::new()) {
            let inventory = Arc::new(inventory);
            assert!(
                !inventory.preview().instances[0].ordinary_import_available,
                "{boundary}"
            );
            assert!(
                inventory
                    .prepare_instance(inventory.fingerprint(), FIRST)
                    .is_err()
            );
        }
        assert_eq!(snapshot(&fixture.baseline), source_before);
        assert_eq!(snapshot(&external), external_before);
    }
}

#[test]
fn ordinary_profile_records_skin_and_music_drift_rejects_prepared_copy() {
    use super::skins::tests::{install, skin};
    for boundary in [
        "skin-png",
        "skin-index",
        "music-file",
        "music-insertion",
        "music-parent",
    ] {
        let fixture = Fixture::new();
        let record = skin(80);
        let png = fixture
            .baseline
            .join(format!("skins/files/{}.png", record.0.texture_key));
        install(&fixture, &[record]);
        let music = fixture.baseline.join("music");
        fs::create_dir(&music).unwrap();
        let track = music.join(crate::music::MUSIC_FILES[0]);
        fs::write(&track, b"original cached track").unwrap();
        let inventory = Arc::new(fixture.capture());
        assert!(inventory.preview().instances[0].ordinary_import_available);
        let prepared = inventory
            .prepare_instance(inventory.fingerprint(), FIRST)
            .unwrap();
        match boundary {
            "skin-png" => fs::write(png, b"changed skin").unwrap(),
            "skin-index" => {
                let mut index = fixture.record("skins/index.json");
                index["skins"][0]["name"] = json!("Changed after admission");
                fixture.write("skins/index.json", &index);
            }
            "music-file" => fs::write(track, b"changed track").unwrap(),
            "music-insertion" => {
                fs::write(music.join("new-effect.keep"), b"preserve new file").unwrap()
            }
            _ => fs::rename(music, fixture.baseline.join("moved-music")).unwrap(),
        }
        let after_change = snapshot(&fixture.baseline);
        assert!(matches!(
            inventory.prepare_instance(inventory.fingerprint(), FIRST),
            Err(ImportError::SourceChanged)
        ));
        let (_root, service) = import_service();
        assert!(service.import_instance(prepared).is_err(), "{boundary}");
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.pending().unwrap().is_empty());
        assert_eq!(snapshot(&fixture.baseline), after_change);
    }
}

pub(crate) fn successful_install_journal() -> Value {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};

    let target = |kind: &str, id: &str| {
        json!({"system":"Application", "kind":kind,
        "id":id, "ownership":"LauncherManaged"})
    };
    let step = |id: &str, phase: &str, result: &str, facts: Value| {
        json!({
            "step_id":id, "phase":phase, "result":result, "changed_target":null,
            "generated_facts":facts, "rollback":"NotApplicable", "guardian_fact_ids":[], "metrics":null
        })
    };
    let checkpoint = |id: &str, kind: &str, version: &str, target_id: &str| {
        let evidence = format!(
            "managed-install-v1.{}.{}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(version.as_bytes())),
            URL_SAFE_NO_PAD.encode([1_u8; 16]),
            URL_SAFE_NO_PAD.encode([2_u8; 16]),
            URL_SAFE_NO_PAD.encode([3_u8; 32]),
            URL_SAFE_NO_PAD.encode([4_u8; 32])
        );
        assert!(
            axial_minecraft::ManagedInstallPublicationEvidenceId::parse(&evidence)
                .unwrap()
                .matches_version_id(version)
        );
        let contract = format!(
            "managed-install-activation-v1.{}",
            URL_SAFE_NO_PAD.encode([5_u8; 32])
        );
        axial_minecraft::ManagedInstallActivationContractId::parse(&contract).unwrap();
        let mut record = step(
            id,
            "Installing",
            "Completed",
            json!([
                format!("install_publication:{kind}"),
                format!("install_publication_version_id:{version}"),
                format!("install_publication_evidence:{evidence}"),
                format!("install_activation_contract:{contract}")
            ]),
        );
        record["changed_target"] = target("Version", target_id);
        record
    };
    let version = "1.20.1";
    let component = axial_minecraft::LoaderComponentId::Fabric;
    let loader = axial_minecraft::installed_version_id_for(component, version, "0.15.11").unwrap();
    let build = axial_minecraft::build_id_for(component, version, "0.15.11");
    let mut entries = Vec::new();
    for (index, kind) in ["vanilla", "loader", "content"].into_iter().enumerate() {
        let operation = format!("op-00000000-0000-4000-8000-{:012x}", index + 11);
        let content = kind == "content";
        let session = format!(
            "{}-{:032x}",
            match kind {
                "content" => "content",
                "loader" => "loader-install",
                _ => "install",
            },
            index + 1
        );
        let (planned, mut completed, subject) = if content {
            (
                step("modify_instance_content", "Planning", "Planned", json!([])),
                vec![step(
                    "content_progress_download",
                    "Downloading",
                    "Completed",
                    json!(["install_phase:download"]),
                )],
                target("Instance", FIRST),
            )
        } else {
            let loader_install = kind == "loader";
            // The predecessor redacts this opaque loader coordinate in targets;
            // its exact identity remains in the planned/publication facts.
            let target_version = if loader_install { "target" } else { version };
            let facts = if loader_install {
                json!([
                    "install_kind:loader",
                    format!("install_version_id:{loader}"),
                    format!("loader_component:{}", component.as_str()),
                    format!("loader_build_id:{build}")
                ])
            } else {
                json!([
                    "install_kind:vanilla",
                    format!("install_version_id:{version}")
                ])
            };
            let mut steps = vec![step(
                "install_progress_recovering",
                "Repairing",
                "Completed",
                json!(["install_phase:recovering"]),
            )];
            if loader_install {
                steps.push(checkpoint(
                    "install_base_publication_committed",
                    "base_committed",
                    version,
                    version,
                ));
                steps.push(checkpoint(
                    "install_child_publication_committed",
                    "child_committed",
                    &loader,
                    target_version,
                ));
            } else {
                steps.push(checkpoint(
                    "install_publication_committed",
                    "committed",
                    version,
                    version,
                ));
            }
            (
                step("install_version", "Planning", "Planned", facts),
                steps,
                target("Version", target_version),
            )
        };
        let mut terminal = step(
            if content {
                "content_progress_done"
            } else {
                "install_progress_done"
            },
            if content { "Downloading" } else { "Completed" },
            "Completed",
            json!(["install_phase:done", "install_done:true"]),
        );
        if content {
            terminal["metrics"] = json!({"kind":"content_download", "values":{
                "checksum_mismatch":0, "metadata_invalid":0, "metadata_missing":0, "interrupted":0,
                "network_failure":1, "permission_failure":0, "promote_failed":0, "provider_failure":2,
                "size_mismatch":0, "temp_discarded":0, "temp_write_failed":0, "written_to_temp":5, "promoted":3
            }});
        }
        completed.push(terminal);
        entries.push(json!({"journal_id":format!("journal-{operation}"), "operation_id":operation,
            "sequence":index+11, "parent_operation_id":null,
            "command":if content {"ModifyInstanceContent"} else {"InstallVersion"}, "intent":{"kind":"generic"},
            "status":"Succeeded", "owner":"Application", "ownership":"LauncherManaged",
            "targets":[target("Session", &session), subject], "planned_steps":[planned], "completed_steps":completed,
            "failure_point":null, "rollback":"NotApplicable", "guardian_diagnosis_ids":[], "outcome":"Succeeded",
            "reconciliation_attempt":null, "reconciliation_terminal":null, "persisted_state_repair_attempt":null,
            "persisted_state_repair_terminal":null, "guardian_install_terminal":null}));
    }
    json!({"schema":"axial.state.operation_journals.v10", "next_sequence":14, "entries":entries})
}

pub(crate) fn rolled_back_install_journal() -> Value {
    let successful = successful_install_journal();
    let mut entries = Vec::new();
    for index in 0..3 {
        let mut entry = successful["entries"][usize::from(index > 0)].clone();
        let operation = format!("op-00000000-0000-4000-8000-{:012x}", index + 21);
        entry["operation_id"] = json!(operation);
        entry["journal_id"] = json!(format!("journal-{operation}"));
        entry["sequence"] = json!(index + 15);
        entry["targets"][0]["id"] = json!(format!(
            "{}-{:032x}",
            if index == 0 {
                "install"
            } else {
                "loader-install"
            },
            index + 21
        ));
        entry["status"] = json!("Failed");
        entry["outcome"] = json!("Failed");
        entry["failure_point"] = json!("install_progress_error");
        let checkpoint_index = if index == 2 { 2 } else { 1 };
        let mut rollback = entry["completed_steps"][checkpoint_index].clone();
        rollback["step_id"] = json!("install_publication_rolled_back");
        rollback["phase"] = json!("RollingBack");
        rollback["rollback"] = json!("Applied");
        rollback["generated_facts"][0] = json!("install_publication:rolled_back");
        rollback["generated_facts"].as_array_mut().unwrap().pop();
        let mut steps = entry["completed_steps"].as_array().unwrap()[..checkpoint_index].to_vec();
        steps.push(rollback);
        let mut terminal = json!({
            "step_id":"install_progress_error", "phase":"Failed", "result":"Failed", "changed_target":null,
            "generated_facts":["install_phase:error", "install_done:true", "install_error:true"],
            "rollback":"NotApplicable", "guardian_fact_ids":[], "metrics":null
        });
        match index {
            0 => {
                terminal["guardian_fact_ids"] = json!(["install_execution_failed"]);
                entry["guardian_diagnosis_ids"] = json!(["install_execution_failed"]);
                entry["guardian_install_terminal"] = json!({"diagnosis_id":"install_execution_failed", "action":"Block", "memory":null});
            }
            1 => {
                // Exact historical Retry carrier from the predecessor's v10 fixture.
                let snapshot: Value = serde_json::from_str(include_str!("../../../../legacy/apps/api/tests/fixtures/guardian/operation-journals-v10.json")).unwrap();
                let retry = &snapshot["entries"][1];
                terminal["guardian_fact_ids"] =
                    retry["completed_steps"][0]["guardian_fact_ids"].clone();
                entry["guardian_diagnosis_ids"] = retry["guardian_diagnosis_ids"].clone();
                entry["guardian_install_terminal"] = retry["guardian_install_terminal"].clone();
            }
            _ => terminal["guardian_fact_ids"] = json!(["download_temp_discarded"]),
        }
        steps.push(terminal);
        entry["completed_steps"] = json!(steps);
        entries.push(entry);
    }
    json!({"schema":"axial.state.operation_journals.v10", "next_sequence":18, "entries":entries})
}

#[tokio::test]
async fn rolled_back_install_import_copies_reopens_and_replays_without_live_authority() {
    for diagnostics in [true, false] {
        let fixture = Fixture::new();
        let mut journal = rolled_back_install_journal();
        journal["entries"][2]["sequence"] = json!(u64::MAX - 1);
        journal["next_sequence"] = json!(u64::MAX);
        if !diagnostics {
            for entry in journal["entries"].as_array_mut().unwrap() {
                entry["guardian_diagnosis_ids"] = json!([]);
                entry["guardian_install_terminal"] = Value::Null;
                for step in entry["completed_steps"].as_array_mut().unwrap() {
                    step["guardian_fact_ids"] = json!([]);
                }
            }
        }
        fixture.write("state/operation-journals.json", &journal);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            inventory.preview().instances[0].ordinary_import_available,
            "diagnostics={diagnostics}"
        );
        assert!(!inventory.preview().cutover_available);
        let (root, service) = import_service();
        service
            .registry()
            .storage()
            .migrate(&[crate::install::queue::MIGRATION])
            .unwrap();
        let imported = service
            .import_instance(
                inventory
                    .prepare_instance(inventory.fingerprint(), FIRST)
                    .unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let destination = imported_path(&service, &imported.id);
        assert_eq!(
            fs::read(destination.join("options.txt")).unwrap(),
            fs::read(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/options.txt"))
            )
            .unwrap()
        );
        let history = service
            .imported_install_history(&imported.id, None)
            .unwrap();
        assert_eq!(history.records.len(), 3);
        for record in &history.records {
            let source = journal["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["operation_id"] == record.operation_id)
                .unwrap();
            let wire = serde_json::to_value(record).unwrap();
            assert!(record.historical);
            assert!(record.instance_id.is_none());
            assert_eq!(
                wire["sequence"],
                source["sequence"].as_u64().unwrap().to_string()
            );
            for field in ["targets", "outcome", "failure_point", "rollback"] {
                assert_eq!(wire[field], source[field]);
            }
            for field in ["planned_steps", "completed_steps"] {
                let mut expected = source[field].clone();
                for step in expected.as_array_mut().unwrap() {
                    step.as_object_mut().unwrap().remove("guardian_fact_ids");
                }
                assert_eq!(wire[field], expected);
            }
            assert!(wire.get("guardian_install_terminal").is_none());
            assert!(wire.get("guardian_diagnosis_ids").is_none());
        }
        let library_id = service
            .directories()
            .library()
            .admit()
            .unwrap()
            .library_id();
        drop(service);
        let service = reopen_import_service(root.path(), library_id);
        assert_eq!(
            service
                .imported_install_history(&imported.id, None)
                .unwrap(),
            history
        );
        fs::write(destination.join("options.txt"), b"keep destination edit").unwrap();
        let copied = snapshot(&destination);
        let repeated = service
            .import_instance(prepare_first(&fixture))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated.id, imported.id);
        assert_eq!(
            service
                .imported_install_history(&imported.id, None)
                .unwrap(),
            history
        );
        let counts = service.registry().storage().read(|db| -> Result<_, crate::storage::StorageError> {
            Ok(db.query_row("SELECT (SELECT COUNT(*) FROM install_history), (SELECT COUNT(*) FROM install_queue), (SELECT COUNT(*) FROM installed_versions)", [], |row| Ok((row.get::<_,u64>(0)?, row.get::<_,u64>(1)?, row.get::<_,u64>(2)?)))?)
        }).unwrap();
        assert_eq!(counts, (3, 0, 0));
        assert_eq!(snapshot(&destination), copied);
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn rolled_back_install_import_refuses_unproven_effects_and_raw_guardian_corruption() {
    for invalid in [
        "running",
        "missing-rollback",
        "order",
        "wrong-version",
        "activation",
        "rollback-state",
        "terminal-first",
        "diagnosis-reference",
        "missing-memory",
        "memory-on-block",
        "window",
        "target",
        "binding",
        "unknown-action",
        "unknown-memory",
        "duplicate-terminal",
        "duplicate-memory",
        "duplicate-target",
    ] {
        let fixture = Fixture::new();
        let mut journal = rolled_back_install_journal();
        match invalid {
            "running" => { journal["entries"][0]["status"] = json!("Running"); journal["entries"][0]["outcome"] = Value::Null; }
            "missing-rollback" => { journal["entries"][0]["completed_steps"].as_array_mut().unwrap().remove(1); }
            "order" => journal["entries"][2]["completed_steps"].as_array_mut().unwrap().swap(1,2),
            "wrong-version" => journal["entries"][1]["completed_steps"][1]["generated_facts"][1] = json!("install_publication_version_id:1.20.2"),
            "activation" => journal["entries"][0]["completed_steps"][1]["generated_facts"].as_array_mut().unwrap().push(successful_install_journal()["entries"][0]["completed_steps"][1]["generated_facts"][3].clone()),
            "rollback-state" => journal["entries"][0]["completed_steps"][1]["rollback"] = json!("NotApplicable"),
            "terminal-first" => journal["entries"][0]["completed_steps"].as_array_mut().unwrap().swap(1,2),
            "diagnosis-reference" => journal["entries"][1]["guardian_diagnosis_ids"] = json!([]),
            "missing-memory" => journal["entries"][1]["guardian_install_terminal"]["memory"] = Value::Null,
            "memory-on-block" => journal["entries"][1]["guardian_install_terminal"]["action"] = json!("Block"),
            "window" => journal["entries"][1]["guardian_install_terminal"]["memory"]["suppression_until"] = json!("2026-08-13T10:06:00.000Z"),
            "target" => journal["entries"][1]["guardian_install_terminal"]["memory"]["target"]["kind"] = json!("Instance"),
            "binding" => journal["entries"][1]["guardian_install_terminal"]["memory"]["binding"] = json!("A".repeat(64)),
            "unknown-action" => journal["entries"][0]["guardian_install_terminal"]["action"] = json!("FutureEffect"),
            "unknown-memory" => journal["entries"][1]["guardian_install_terminal"]["memory"]["future"] = json!(true),
            _ => {}
        }
        journal["entries"].as_array_mut().unwrap().extend(
            terminal_rules_journal()["entries"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        fixture.write("state/operation-journals.json", &journal);
        let raw = serde_json::to_string(&journal).unwrap();
        let raw = match invalid {
            "duplicate-terminal" => raw.replacen("\"action\":\"Block\"", "\"action\":\"Block\",\"action\":\"Block\"", 1),
            "duplicate-memory" => raw.replacen("\"observed_at\":\"2026-08-13T10:00:00.000Z\"", "\"observed_at\":\"2026-08-13T10:00:00.000Z\",\"observed_at\":\"2026-08-13T10:00:00.000Z\"", 1),
            "duplicate-target" => raw.replacen("\"kind\":\"Artifact\"", "\"kind\":\"Artifact\",\"kind\":\"Artifact\"", 1),
            _ => raw,
        };
        if invalid.starts_with("duplicate-") {
            assert_ne!(raw, serde_json::to_string(&journal).unwrap(), "{invalid}");
        }
        fs::write(
            fixture.baseline.join("state/operation-journals.json"),
            raw.as_bytes(),
        )
        .unwrap();
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{invalid}"
        );
        assert!(!inventory.preview().rules_import_available, "{invalid}");
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(
            inventory
                .record_bytes("profile/state/operation-journals.json")
                .unwrap(),
            raw.as_bytes()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn rolled_back_install_import_keeps_source_fences() {
    let fixture = Fixture::new();
    let mut journal = rolled_back_install_journal();
    fixture.write("state/operation-journals.json", &journal);
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(inventory.fingerprint(), FIRST)
        .unwrap();
    journal["entries"][1]["guardian_install_terminal"]["memory"]["observed_at"] =
        json!("2026-08-13T11:00:00.000Z");
    journal["entries"][1]["guardian_install_terminal"]["memory"]["suppression_until"] =
        json!("2026-08-13T11:05:00.000Z");
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    assert!(matches!(
        inventory.revalidate(),
        Err(ImportError::SourceChanged)
    ));
    let (_root, service) = import_service();
    assert!(service.import_instance(prepared).is_err());
    assert!(service.registry().list().unwrap().is_empty());
    assert!(service.pending().unwrap().is_empty());
    assert_eq!(snapshot(&fixture.baseline), before);
}

pub(crate) fn cancelled_content_initialization_journal() -> Value {
    let mut entry = successful_install_journal()["entries"][2].clone();
    let operation = "op-00000000-0000-4000-8000-00000000000e";
    entry["journal_id"] = json!(format!("journal-{operation}"));
    entry["operation_id"] = json!(operation);
    entry["sequence"] = json!(14);
    entry["targets"][0]["id"] = json!("content-0000000000000000000000000000000e");
    entry["status"] = json!("Failed");
    entry["outcome"] = json!("Failed");
    entry["failure_point"] = json!("content_initialization_cancelled");
    // Reservation cleanup runs before worker hand-off and records no metrics.
    entry["completed_steps"] = json!([{
        "step_id":"content_progress_initializing", "phase":"Failed", "result":"Failed",
        "changed_target":null,
        "generated_facts":["install_phase:initializing", "install_done:true", "install_error:true"],
        "rollback":"NotApplicable", "guardian_fact_ids":[], "metrics":null
    }]);
    json!({"schema":"axial.state.operation_journals.v10", "next_sequence":15, "entries":[entry]})
}

#[tokio::test]
async fn content_initialization_cancelled_import_copies_reopens_and_replays_exact_history() {
    let fixture = Fixture::new();
    fixture.two_instances();
    let mut journal = cancelled_content_initialization_journal();
    journal["entries"][0]["sequence"] = json!(u64::MAX - 1);
    journal["next_sequence"] = json!(u64::MAX);
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    let inventory = Arc::new(fixture.capture());
    let preview = inventory.preview();
    assert!(
        preview
            .instances
            .iter()
            .all(|row| row.ordinary_import_available)
    );
    assert!(
        preview
            .blockers
            .contains(&ImportBlocker::UnsettledOperation)
    );
    assert!(!preview.cutover_available);
    let (root, service) = import_service();
    service
        .registry()
        .storage()
        .migrate(&[crate::install::queue::MIGRATION])
        .unwrap();
    let second = service
        .import_instance(
            inventory
                .prepare_instance(&preview.fingerprint, SECOND)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert!(
        service
            .imported_install_history(&second.id, None)
            .unwrap()
            .records
            .is_empty()
    );
    let imported = service
        .import_instance(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let destination = imported_path(&service, &imported.id);
    assert_eq!(
        fs::read(destination.join("options.txt")).unwrap(),
        fs::read(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt"))
        )
        .unwrap()
    );
    let history = service
        .imported_install_history(&imported.id, None)
        .unwrap();
    assert_eq!(history.records.len(), 1);
    assert!(history.next_after.is_none());
    let record = &history.records[0];
    let wire = serde_json::to_value(record).unwrap();
    let source = &journal["entries"][0];
    assert!(record.historical);
    assert_eq!(record.instance_id.as_deref(), Some(imported.id.as_str()));
    assert_eq!(wire["sequence"], (u64::MAX - 1).to_string());
    for field in [
        "journal_id",
        "operation_id",
        "command",
        "targets",
        "outcome",
        "failure_point",
        "rollback",
    ] {
        assert_eq!(wire[field], source[field], "{field}");
    }
    // Guardian-only empty lists are not part of the public history projection.
    for field in ["planned_steps", "completed_steps"] {
        let mut expected = source[field].clone();
        expected[0]
            .as_object_mut()
            .unwrap()
            .remove("guardian_fact_ids");
        assert_eq!(wire[field], expected, "{field}");
    }
    let library_id = service
        .directories()
        .library()
        .admit()
        .unwrap()
        .library_id();
    drop(service);
    let service = reopen_import_service(root.path(), library_id);
    assert_eq!(
        service
            .imported_install_history(&imported.id, None)
            .unwrap(),
        history
    );
    fs::write(
        destination.join("options.txt"),
        b"destination edit after import",
    )
    .unwrap();
    let copied = snapshot(&destination);
    let repeated = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(
        service
            .imported_install_history(&imported.id, None)
            .unwrap(),
        history
    );
    assert!(
        service
            .imported_install_history(&second.id, None)
            .unwrap()
            .records
            .is_empty()
    );
    let counts = service.registry().storage().read(|db| -> Result<_, crate::storage::StorageError> {
        Ok(db.query_row("SELECT (SELECT COUNT(*) FROM install_history), (SELECT COUNT(*) FROM install_queue), (SELECT COUNT(*) FROM installed_versions)", [], |row| Ok((row.get::<_,u64>(0)?, row.get::<_,u64>(1)?, row.get::<_,u64>(2)?)))?)
    }).unwrap();
    assert_eq!(counts, (1, 0, 0));
    assert_eq!(snapshot(&destination), copied);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn content_initialization_cancelled_import_preserves_mixed_success_and_rules_history() {
    let fixture = Fixture::new();
    let (_, key) = seed_rules_source(&fixture);
    let mut journal = cancelled_content_initialization_journal();
    for other in [successful_install_journal(), terminal_rules_journal()] {
        journal["entries"]
            .as_array_mut()
            .unwrap()
            .extend(other["entries"].as_array().unwrap().iter().cloned());
    }
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    assert!(preview.rules_import_available);
    assert!(!preview.instances[0].ordinary_import_available);
    assert!(!preview.cutover_available);
    let (root, service) = import_service();
    let rules = rules_import_owner(root.path(), &service, key);
    let pin = service
        .directories()
        .library()
        .admit_application_root()
        .unwrap();
    let result = previews
        .prepare_rules(&preview.fingerprint)
        .unwrap()
        .commit(
            &rules,
            &pin,
            &model::RulesImportRequest {
                fingerprint: preview.fingerprint.clone(),
                rules_import_id: preview.rules_import_id,
            },
            &crate::tasks::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.receipt.refresh_history.len(), 4);
    let count: u64 = service
        .registry()
        .storage()
        .read(|db| -> Result<_, crate::storage::StorageError> {
            Ok(db.query_row("SELECT COUNT(*) FROM install_history", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(
        count, 0,
        "rules publication does not publish install history"
    );
    assert!(previews.current_with_rules(&rules).unwrap().instances[0].ordinary_import_available);
    let imported = service
        .import_instance(
            previews
                .prepare_instance_with_rules(&preview.fingerprint, FIRST, &rules)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let history = service
        .imported_install_history(&imported.id, None)
        .unwrap();
    assert_eq!(history.records.len(), 4);
    assert_eq!(
        history
            .records
            .iter()
            .filter(|row| row.outcome == "Succeeded")
            .count(),
        3
    );
    let failed = history
        .records
        .iter()
        .find(|row| row.outcome == "Failed")
        .unwrap();
    assert_eq!(
        serde_json::to_value(failed).unwrap()["failure_point"],
        "content_initialization_cancelled"
    );
    assert_eq!(failed.instance_id.as_deref(), Some(imported.id.as_str()));
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[test]
fn content_initialization_cancelled_import_refuses_worker_effects_and_raw_malformed_evidence() {
    for invalid in [
        "cancelled",
        "missing-failure",
        "worker-failure",
        "wrong-instance",
        "extra-step",
        "metrics",
        "changed-target",
        "facts-order",
        "missing-fact",
        "unknown-step",
        "duplicate-failure",
        "duplicate-phase",
        "reconciliation",
    ] {
        let fixture = Fixture::new();
        let mut journal = cancelled_content_initialization_journal();
        let entry = &mut journal["entries"][0];
        match invalid {
            "cancelled" => {
                entry["status"] = json!("Cancelled");
                entry["outcome"] = json!("Cancelled");
            }
            "missing-failure" => entry["failure_point"] = Value::Null,
            "worker-failure" => entry["failure_point"] = json!("operation_worker_stopped"),
            "wrong-instance" => entry["targets"][1]["id"] = json!(SECOND),
            "extra-step" => entry["completed_steps"].as_array_mut().unwrap().insert(
                0,
                successful_install_journal()["entries"][2]["completed_steps"][0].clone(),
            ),
            "metrics" => {
                entry["completed_steps"][0]["metrics"] = successful_install_journal()["entries"][2]
                    ["completed_steps"][1]["metrics"]
                    .clone()
            }
            "changed-target" => {
                entry["completed_steps"][0]["changed_target"] = entry["targets"][1].clone()
            }
            "facts-order" => entry["completed_steps"][0]["generated_facts"]
                .as_array_mut()
                .unwrap()
                .swap(1, 2),
            "missing-fact" => {
                entry["completed_steps"][0]["generated_facts"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
            "unknown-step" => entry["completed_steps"][0]["effect"] = json!(true),
            "reconciliation" => entry["reconciliation_attempt"] = json!({"retained_effect":true}),
            _ => {}
        }
        journal["entries"].as_array_mut().unwrap().extend(
            terminal_rules_journal()["entries"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        fixture.write("state/operation-journals.json", &journal);
        let raw = serde_json::to_string(&journal).unwrap();
        let raw = match invalid {
            "duplicate-failure" => raw.replacen("\"failure_point\":\"content_initialization_cancelled\"", "\"failure_point\":\"content_initialization_cancelled\",\"failure_point\":\"content_initialization_cancelled\"", 1),
            "duplicate-phase" => raw.replacen("\"phase\":\"Failed\"", "\"phase\":\"Failed\",\"phase\":\"Failed\"", 1),
            _ => raw,
        };
        fs::write(
            fixture.baseline.join("state/operation-journals.json"),
            raw.as_bytes(),
        )
        .unwrap();
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(!preview.instances[0].ordinary_import_available, "{invalid}");
        assert!(!preview.rules_import_available, "{invalid}");
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(
            inventory
                .record_bytes("profile/state/operation-journals.json")
                .unwrap(),
            raw.as_bytes()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn content_initialization_cancelled_import_keeps_source_fences() {
    let fixture = Fixture::new();
    let mut journal = cancelled_content_initialization_journal();
    fixture.write("state/operation-journals.json", &journal);
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(inventory.fingerprint(), FIRST)
        .unwrap();
    journal["entries"][0]["sequence"] = json!(15);
    journal["next_sequence"] = json!(16);
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    assert!(matches!(
        inventory.revalidate(),
        Err(ImportError::SourceChanged)
    ));
    let (_root, service) = import_service();
    assert!(service.import_instance(prepared).is_err());
    assert!(service.registry().list().unwrap().is_empty());
    assert!(service.pending().unwrap().is_empty());
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn successful_install_history_import_allows_real_copy_and_reopen() {
    let fixture = Fixture::new();
    fixture.two_instances();
    let mut journal = successful_install_journal();
    journal["entries"][1]["sequence"] = json!(9_007_199_254_740_993_u64);
    journal["entries"][2]["sequence"] = json!(u64::MAX - 1);
    journal["entries"][2]["completed_steps"][1]["metrics"]["values"]["promoted"] = json!(u64::MAX);
    journal["next_sequence"] = json!(u64::MAX);
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    let inventory = Arc::new(fixture.capture());
    let preview = inventory.preview();
    assert!(
        preview
            .instances
            .iter()
            .all(|instance| instance.ordinary_import_available)
    );
    assert!(
        preview
            .blockers
            .contains(&ImportBlocker::UnsettledOperation)
    );
    assert!(!preview.cutover_available);
    let (root, service) = import_service();
    service
        .registry()
        .storage()
        .migrate(&[crate::install::queue::MIGRATION])
        .unwrap();
    // An unrelated source instance publishes only the source-global versions.
    let second = service
        .import_instance(
            inventory
                .prepare_instance(&preview.fingerprint, SECOND)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let global = service.imported_install_history(&second.id, None).unwrap();
    assert_eq!(global.records.len(), 2);
    assert!(
        global
            .records
            .iter()
            .all(|record| record.instance_id.is_none())
    );
    let imported = service
        .import_instance(
            inventory
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let destination = imported_path(&service, &imported.id);
    assert_eq!(
        fs::read(destination.join("options.txt")).unwrap(),
        fs::read(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/options.txt"))
        )
        .unwrap()
    );
    let copied = snapshot(&destination);
    let history = service
        .imported_install_history(&imported.id, None)
        .unwrap();
    assert_eq!(history.records.len(), 3);
    assert!(history.next_after.is_none());
    for record in &history.records {
        let original = journal["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["operation_id"] == record.operation_id)
            .unwrap();
        assert!(record.historical);
        assert_eq!(
            record.sequence,
            original["sequence"].as_u64().unwrap().to_string()
        );
        assert_eq!(
            serde_json::to_value(&record.targets).unwrap(),
            original["targets"]
        );
        assert_eq!(
            record.planned_steps[0].generated_facts,
            serde_json::from_value::<Vec<String>>(
                original["planned_steps"][0]["generated_facts"].clone()
            )
            .unwrap()
        );
        assert_eq!(
            record
                .completed_steps
                .iter()
                .map(|step| step.step_id.as_str())
                .collect::<Vec<_>>(),
            original["completed_steps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|step| step["step_id"].as_str().unwrap())
                .collect::<Vec<_>>()
        );
        if record.command == "ModifyInstanceContent" {
            assert_eq!(record.instance_id.as_deref(), Some(imported.id.as_str()));
            let wire = serde_json::to_value(record).unwrap();
            assert_eq!(wire["sequence"], (u64::MAX - 1).to_string());
            assert_eq!(
                wire["completed_steps"][1]["metrics"]["values"]["promoted"],
                u64::MAX.to_string()
            );
            assert_eq!(
                wire["completed_steps"][1]["metrics"]["values"]["provider_failure"],
                "2"
            );
        } else {
            assert!(record.instance_id.is_none());
        }
    }
    assert_eq!(
        service.imported_install_history(&second.id, None).unwrap(),
        global
    );
    let library_id = service
        .directories()
        .library()
        .admit()
        .unwrap()
        .library_id();
    drop(service);
    let service = reopen_import_service(root.path(), library_id);
    let repeated = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(
        service
            .imported_install_history(&imported.id, None)
            .unwrap(),
        history
    );
    assert_eq!(
        service.imported_install_history(&second.id, None).unwrap(),
        global
    );
    let counts = service.registry().storage().read(|db| -> Result<_, crate::storage::StorageError> {
        Ok(db.query_row("SELECT (SELECT COUNT(*) FROM install_history), (SELECT COUNT(*) FROM install_queue), (SELECT COUNT(*) FROM installed_versions)", [], |row| Ok((row.get::<_,u64>(0)?, row.get::<_,u64>(1)?, row.get::<_,u64>(2)?)))?)
    }).unwrap();
    assert_eq!(counts, (3, 0, 0));
    assert_eq!(snapshot(&destination), copied);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn rolled_back_install_import_preserves_mixed_rules_and_success_history() {
    let fixture = Fixture::new();
    let (_, key) = seed_rules_source(&fixture);
    let mut journal = successful_install_journal();
    journal["next_sequence"] = json!(18);
    journal["entries"].as_array_mut().unwrap().extend(
        rolled_back_install_journal()["entries"]
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
    );
    journal["entries"].as_array_mut().unwrap().extend(
        terminal_rules_journal()["entries"]
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
    );
    journal["entries"].as_array_mut().unwrap().extend(
        terminal_performance_journal()["entries"]
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
    );
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    assert!(preview.rules_import_available);
    previews.prepare_rules(&preview.fingerprint).unwrap();
    assert!(
        !preview.instances[0].ordinary_import_available,
        "rules still require explicit publication"
    );
    assert!(!preview.cutover_available);
    let (root, service) = import_service();
    let rules = rules_import_owner(root.path(), &service, key);
    let pin = service
        .directories()
        .library()
        .admit_application_root()
        .unwrap();
    let result = previews
        .prepare_rules(&preview.fingerprint)
        .unwrap()
        .commit(
            &rules,
            &pin,
            &model::RulesImportRequest {
                fingerprint: preview.fingerprint.clone(),
                rules_import_id: preview.rules_import_id,
            },
            &crate::tasks::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.receipt.refresh_history.len(), 4);
    assert!(historical_commands(&service).is_empty());
    let count: u64 = service
        .registry()
        .storage()
        .read(|db| -> Result<_, crate::storage::StorageError> {
            Ok(db.query_row("SELECT COUNT(*) FROM install_history", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(
        count, 0,
        "rules publication must not publish instance or version history"
    );
    assert!(previews.current_with_rules(&rules).unwrap().instances[0].ordinary_import_available);
    let prepared = previews
        .prepare_instance_with_rules(&preview.fingerprint, FIRST, &rules)
        .unwrap();
    let imported = service
        .import_instance(prepared)
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        service
            .imported_install_history(&imported.id, None)
            .unwrap()
            .records
            .len(),
        6
    );
    assert_eq!(historical_commands(&service).len(), 6);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[test]
fn successful_install_history_import_refuses_unproven_and_raw_malformed_evidence() {
    for invalid in [
        "running",
        "failed",
        "unknown-command",
        "missing-checkpoint",
        "checkpoint-order",
        "checkpoint-version",
        "activation",
        "terminal-first",
        "wrong-instance",
        "content-phase",
        "metrics-kind",
        "missing-counter",
        "string-counter",
        "unknown-counter",
        "duplicate-metrics-kind",
        "duplicate-counter",
        "overflow-counter",
        "unknown-envelope",
        "reconciliation",
    ] {
        let fixture = Fixture::new();
        let mut journal = successful_install_journal();
        match invalid {
            "running" => {
                journal["entries"][0]["status"] = json!("Running");
                journal["entries"][0]["outcome"] = Value::Null;
            }
            "failed" => {
                journal["entries"][0]["status"] = json!("Failed");
                journal["entries"][0]["outcome"] = json!("Failed");
            }
            "unknown-command" => journal["entries"][0]["command"] = json!("FutureInstall"),
            "missing-checkpoint" => {
                journal["entries"][0]["completed_steps"]
                    .as_array_mut()
                    .unwrap()
                    .remove(1);
            }
            "checkpoint-order" => journal["entries"][1]["completed_steps"]
                .as_array_mut()
                .unwrap()
                .swap(1, 2),
            "checkpoint-version" => {
                journal["entries"][0]["completed_steps"][1]["generated_facts"][1] =
                    json!("install_publication_version_id:1.20.2")
            }
            "activation" => {
                journal["entries"][0]["completed_steps"][1]["generated_facts"][3] =
                    json!("install_activation_contract:invalid")
            }
            "terminal-first" => journal["entries"][0]["completed_steps"]
                .as_array_mut()
                .unwrap()
                .swap(0, 2),
            "wrong-instance" => journal["entries"][2]["targets"][1]["id"] = json!(SECOND),
            "content-phase" => {
                journal["entries"][2]["completed_steps"][1]["phase"] = json!("Completed")
            }
            "metrics-kind" => {
                journal["entries"][2]["completed_steps"][1]["metrics"]["kind"] =
                    json!("tier2_integrity")
            }
            "missing-counter" => {
                journal["entries"][2]["completed_steps"][1]["metrics"]["values"]
                    .as_object_mut()
                    .unwrap()
                    .remove("promoted");
            }
            "string-counter" => {
                journal["entries"][2]["completed_steps"][1]["metrics"]["values"]["promoted"] =
                    json!("3")
            }
            "unknown-counter" => {
                journal["entries"][2]["completed_steps"][1]["metrics"]["values"]["future"] =
                    json!(1)
            }
            "unknown-envelope" => journal["entries"][0]["effect"] = json!(true),
            "reconciliation" => {
                journal["entries"][0]["reconciliation_attempt"] = json!({"retained_effect":true})
            }
            _ => {}
        }
        journal["entries"].as_array_mut().unwrap().extend(
            terminal_rules_journal()["entries"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        fixture.write("state/operation-journals.json", &journal);
        let raw = serde_json::to_string(&journal).unwrap();
        let raw = match invalid {
            "duplicate-metrics-kind" => raw.replacen(
                "\"kind\":\"content_download\"",
                "\"kind\":\"content_download\",\"kind\":\"content_download\"",
                1,
            ),
            "duplicate-counter" => {
                raw.replacen("\"promoted\":3", "\"promoted\":3,\"promoted\":3", 1)
            }
            "overflow-counter" => {
                raw.replacen("\"promoted\":3", "\"promoted\":18446744073709551616", 1)
            }
            _ => raw,
        };
        fs::write(
            fixture.baseline.join("state/operation-journals.json"),
            raw.as_bytes(),
        )
        .unwrap();
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let preview = inventory.preview();
        assert!(!preview.instances[0].ordinary_import_available, "{invalid}");
        assert!(!preview.rules_import_available, "{invalid}");
        assert!(
            inventory
                .prepare_instance(inventory.fingerprint(), FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(
            inventory
                .record_bytes("profile/state/operation-journals.json")
                .unwrap(),
            raw.as_bytes()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn successful_install_history_import_keeps_source_fences() {
    let fixture = Fixture::new();
    let mut journal = successful_install_journal();
    fixture.write("state/operation-journals.json", &journal);
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(inventory.fingerprint(), FIRST)
        .unwrap();
    journal["entries"][0]["sequence"] = json!(33);
    journal["next_sequence"] = json!(34);
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    assert!(matches!(
        inventory.revalidate(),
        Err(ImportError::SourceChanged)
    ));
    let (_root, service) = import_service();
    assert!(service.import_instance(prepared).is_err());
    assert!(service.registry().list().unwrap().is_empty());
    assert!(service.pending().unwrap().is_empty());
    assert_eq!(snapshot(&fixture.baseline), before);
}

pub(crate) fn terminal_performance_journal() -> Value {
    let cases = [
        (
            "install",
            "install",
            "old-composition",
            "Unavailable",
            json!({
                "outcome":"succeeded", "prepared":{"result_target_id":"old-composition", "proof":{
                    "proof":"install_plan", "graph_sha512":"a".repeat(128), "artifact_count":1_000_000, "aggregate_bytes":u64::MAX
                }}, "changed_target":true, "rollback":"Available"
            }),
        ),
        (
            "rollback",
            "rollback",
            "performance_rollback_snapshot",
            "Unavailable",
            json!({"outcome":"failed_before_effect", "error":"Snapshot was unavailable"}),
        ),
        (
            "remove",
            "remove",
            "old-composition",
            "Available",
            json!({
                "outcome":"failed_after_effect", "prepared":{"result_target_id":"old-composition", "proof":{
                    "proof":"remove_current", "graph_sha512":"b".repeat(128), "artifact_count":17
                }}, "changed_target":true, "rollback":"Unavailable", "error":"Removal failed after effect"
            }),
        ),
        (
            "install",
            "remove",
            "performance_composition_lock",
            "Unavailable",
            json!({"outcome":"abandoned_before_effect"}),
        ),
        (
            "rollback",
            "rollback",
            "old-composition",
            "Available",
            json!({
                "outcome":"succeeded", "prepared":{"result_target_id":"old-composition", "proof":{
                    "proof":"rollback_snapshot", "snapshot_id":"pruned-snapshot", "target":"managed_composition", "artifact_count":2
                }}, "changed_target":true, "rollback":"Applied"
            }),
        ),
        (
            "install",
            "remove",
            "performance_composition_lock",
            "Unavailable",
            json!({
                "outcome":"succeeded", "prepared":{"result_target_id":"performance_composition_lock", "proof":{"proof":"managed_state_absent"}},
                "changed_target":false, "rollback":"Unavailable"
            }),
        ),
    ];
    let mut entries = Vec::new();
    for (index, (requested, action, target, rollback, terminal)) in cases.into_iter().enumerate() {
        let operation = format!("op-00000000-0000-4000-8000-{:012x}", index + 1);
        let outcome = terminal["outcome"].as_str().unwrap();
        let status = match outcome {
            "succeeded" => "Succeeded",
            "abandoned_before_effect" => "Cancelled",
            _ => "Failed",
        };
        let failure = match status {
            "Succeeded" => Value::Null,
            "Cancelled" => json!("performance_operation_abandoned_before_effect"),
            _ => json!("performance_operation_failed"),
        };
        entries.push(json!({
            "journal_id":format!("journal-{operation}"), "operation_id":operation, "sequence":index + 1,
            "command":"ApplyPerformancePlan", "intent":{
                "kind":"performance", "intent":{"instance_id":FIRST, "requested_action":requested, "action":action,
                    "base_target_id":target, "rollback":rollback, "game_version":"1.20.1", "loader":"fabric", "mode":"managed"},
                "phase":{"phase":"terminal", "terminal":terminal},
                "created_at":"2024-02-29T12:34:56.000Z", "updated_at":"2024-02-29T12:35:56.000Z"
            }, "status":status, "owner":"Application", "ownership":"CompositionManaged",
            "targets":[
                {"system":"State", "kind":"Instance", "id":FIRST, "ownership":"CompositionManaged"},
                {"system":"Performance", "kind":"PerformanceComposition", "id":target, "ownership":"CompositionManaged"}
            ], "planned_steps":[], "completed_steps":[], "failure_point":failure,
            "rollback":terminal.get("rollback").cloned().unwrap_or_else(|| json!(rollback)),
            "guardian_diagnosis_ids":[], "outcome":status,
            "reconciliation_attempt":null, "reconciliation_terminal":null,
            "persisted_state_repair_attempt":null, "persisted_state_repair_terminal":null,
            "guardian_install_terminal":null
        }));
    }
    // The predecessor serializes its operation-ID map, not chronological order.
    entries.reverse();
    json!({"schema":"axial.state.operation_journals.v10", "next_sequence":7, "entries":entries})
}

fn terminal_rules_journal() -> Value {
    let cache = json!({"system":"Performance","kind":"Config","id":"performance_rules_cache","ownership":"LauncherManaged"});
    let entries: Vec<_> = (0..4).map(|index| {
        let operation = format!("op-{}", uuid::Uuid::new_v4());
        let success = index < 2;
        let step = |result: &str, changed: bool| json!({"step_id":"refresh_remote_rules","phase":"Running","result":result,"changed_target":if changed { cache.clone() } else { Value::Null },"generated_facts":[],"rollback":"NotApplicable","guardian_fact_ids":[],"metrics":null});
        json!({"journal_id":format!("journal-{operation}"),"operation_id":operation,"sequence":index+7,"parent_operation_id":null,"command":"RefreshPerformanceRules","intent":{"kind":"generic"},"status":if success {"Succeeded"} else {"Failed"},"owner":"Application","ownership":"LauncherManaged",
            "targets":[{"system":"Performance","kind":"NetworkResource","id":"performance_rules_remote_source","ownership":"ExternalProviderDerived"},cache],
            "planned_steps":[step("Planned",false)],"completed_steps":[step(if success {"Completed"} else {"Failed"},index==1)],"failure_point":match index { 2 => json!("refresh_remote_rules"), 3 => json!("refresh_rules_journal_reconciliation"), _ => Value::Null },"rollback":"NotApplicable","guardian_diagnosis_ids":[],"outcome":if success {"Succeeded"} else {"Failed"},"reconciliation_attempt":null,"reconciliation_terminal":null,"persisted_state_repair_attempt":null,"persisted_state_repair_terminal":null,"guardian_install_terminal":null})
    }).collect();
    json!({"schema":"axial.state.operation_journals.v10","next_sequence":11,"entries":entries})
}

fn rules_import_owner(
    root: &Path,
    service: &crate::instances::create::InstanceService,
    key: String,
) -> crate::performance::rules::PerformanceRules {
    use crate::performance::rules::{IMPORT_MIGRATION, PerformanceRules};
    service
        .registry()
        .storage()
        .migrate(&[IMPORT_MIGRATION])
        .unwrap();
    PerformanceRules::with_remote(
        Arc::new(crate::storage::MetadataStore::open(root.join("metadata.sqlite")).unwrap()),
        Some("https://example.invalid/rules".into()),
        Some(key),
    )
    .unwrap()
}

fn seed_rules_source(fixture: &Fixture) -> (Vec<u8>, String) {
    let (bytes, key) = crate::performance::rules::tests::signed_cache("2001-01-01T00:00:00Z");
    fs::create_dir_all(fixture.baseline.join("performance")).unwrap();
    fs::write(
        fixture.baseline.join("performance/rules-cache.json"),
        &bytes,
    )
    .unwrap();
    (bytes, key)
}

#[tokio::test]
async fn rules_import_unlocks_exact_managed_publication_and_checks_fact_on_recovery_and_replay() {
    use crate::{
        performance::rules::RulesImportReceipt, storage::StorageError, tasks::CancellationToken,
    };
    use sha2::Digest;
    let fixture = Fixture::new();
    seed_managed_source(&fixture).await;
    let provenance = seed_content_source(&fixture);
    let (bytes, key) = seed_rules_source(&fixture);
    let mut journal = terminal_performance_journal();
    journal["entries"].as_array_mut().unwrap().extend(
        terminal_rules_journal()["entries"]
            .as_array()
            .unwrap()
            .clone(),
    );
    journal["next_sequence"] = json!(11);
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    let (root, service) = import_service();
    let rules = rules_import_owner(root.path(), &service, key.clone());
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    assert!(preview.rules_import_available && !preview.instances[0].ordinary_import_available);
    assert!(!previews.current_with_rules(&rules).unwrap().instances[0].ordinary_import_available);
    assert!(
        previews
            .prepare_instance_with_rules(&preview.fingerprint, FIRST, &rules)
            .is_err()
    );
    let input = previews.prepare_rules(&preview.fingerprint).unwrap();
    let request = model::RulesImportRequest {
        fingerprint: preview.fingerprint.clone(),
        rules_import_id: preview.rules_import_id.clone(),
    };
    let pin = service
        .directories()
        .library()
        .admit_application_root()
        .unwrap();
    let result = input
        .commit(&rules, &pin, &request, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !result.already_imported && result.stored_cache_matches_import && !result.cutover_available
    );
    assert_eq!(result.receipt.refresh_history.len(), 4);
    assert_eq!(
        result.receipt.cache_sha256,
        Some(hex::encode(sha2::Sha256::digest(&bytes)))
    );
    let current = previews.current_with_rules(&rules).unwrap();
    assert_eq!(current.fingerprint, preview.fingerprint);
    assert!(current.instances[0].ordinary_import_available && !current.cutover_available);
    let prepared = previews
        .prepare_instance_with_rules(&preview.fingerprint, FIRST, &rules)
        .unwrap();
    let retry = prepared.clone();
    let id = prepared.instance().id.clone();
    service.registry().storage().transaction(|tx| -> Result<_, StorageError> {
        tx.execute_batch("CREATE TRIGGER reject_rules_instance BEFORE UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(ABORT,'injected'); END;")?; Ok(())
    }).unwrap();
    assert!(
        service
            .import_instance(prepared)
            .unwrap()
            .join()
            .await
            .unwrap()
            .is_err()
    );
    assert!(service.registry().list().unwrap().is_empty());
    assert!(historical_commands(&service).is_empty());
    assert_eq!(
        fs::read(imported_path(&service, &id).join("axial.content.json")).unwrap(),
        provenance
    );
    let fact: (String, String, String, Vec<u8>) = service
        .registry()
        .storage()
        .read(|db| -> Result<_, StorageError> {
            Ok(db.query_row(
                "SELECT source_id,fingerprint,import_id,receipt FROM performance_rules_imports",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<RulesImportReceipt>(&fact.3).unwrap(),
        result.receipt
    );
    service
        .registry()
        .storage()
        .transaction(|tx| -> Result<_, StorageError> {
            tx.execute_batch(
                "DROP TRIGGER reject_rules_instance; DELETE FROM performance_rules_imports;",
            )?;
            Ok(())
        })
        .unwrap();
    assert!(!previews.current_with_rules(&rules).unwrap().instances[0].ordinary_import_available);
    assert!(
        service
            .import_instance(retry)
            .unwrap()
            .join()
            .await
            .unwrap()
            .is_err(),
        "already prepared recovery still rechecks the durable fact"
    );
    assert!(service.registry().list().unwrap().is_empty());
    service.registry().storage().transaction(|tx| -> Result<_, StorageError> { tx.execute("INSERT INTO performance_rules_imports(source_id,fingerprint,import_id,receipt) VALUES(?1,?2,?3,?4)", crate::storage::rusqlite::params![fact.0,fact.1,fact.2,fact.3])?; Ok(()) }).unwrap();
    let library_id = service
        .directories()
        .library()
        .admit()
        .unwrap()
        .library_id();
    drop(pin);
    drop(rules);
    drop(service);
    let service = reopen_import_service(root.path(), library_id);
    let rules = rules_import_owner(root.path(), &service, key);
    previews.admit(fixture.capture()).unwrap();
    let prepared = previews
        .prepare_instance_with_rules(&preview.fingerprint, FIRST, &rules)
        .unwrap();
    let completed_retry = prepared.clone();
    assert_eq!(
        service
            .import_instance(prepared)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
            .id,
        id
    );
    assert_eq!(historical_commands(&service).len(), 6);
    previews.forget().unwrap();
    assert_eq!(
        rules_status(&rules, &request.rules_import_id)
            .unwrap()
            .receipt,
        Some(result.receipt)
    );
    assert_eq!(
        service
            .import_instance(completed_retry.clone())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
            .id,
        id
    );
    service
        .registry()
        .storage()
        .transaction(|tx| -> Result<_, StorageError> {
            tx.execute("DELETE FROM performance_rules_imports", [])?;
            Ok(())
        })
        .unwrap();
    assert!(
        service
            .import_instance(completed_retry)
            .unwrap()
            .join()
            .await
            .unwrap()
            .is_err(),
        "completed instance replay must not resurrect a missing rules fact"
    );
    assert_eq!(service.registry().list().unwrap().len(), 1);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn rules_import_source_change_cancellation_and_forgotten_preview_preserve_boundaries() {
    use crate::tasks::CancellationToken;
    for boundary in ["changed", "cancelled", "forgotten"] {
        let fixture = Fixture::new();
        let (_, key) = seed_rules_source(&fixture);
        let (root, service) = import_service();
        let rules = rules_import_owner(root.path(), &service, key);
        let previews = ImportPreviews::new();
        let preview = previews.admit(fixture.capture()).unwrap();
        let input = previews.prepare_rules(&preview.fingerprint).unwrap();
        let request = model::RulesImportRequest {
            fingerprint: preview.fingerprint,
            rules_import_id: preview.rules_import_id,
        };
        let cancel = CancellationToken::new();
        match boundary {
            "changed" => fs::write(fixture.baseline.join("config.json"), b"{}").unwrap(),
            "cancelled" => {
                cancel.cancel();
            }
            _ => previews.forget().unwrap(),
        }
        let before = snapshot(&fixture.baseline);
        let pin = service
            .directories()
            .library()
            .admit_application_root()
            .unwrap();
        let result = input.commit(&rules, &pin, &request, &cancel).await;
        if boundary == "forgotten" {
            assert!(
                result.is_ok(),
                "accepted input outlives preview cancellation"
            );
        } else {
            assert!(result.is_err());
            assert!(
                rules_status(&rules, &request.rules_import_id)
                    .unwrap()
                    .receipt
                    .is_none()
            );
        }
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn rules_import_retains_large_source_sequences_exactly_through_receipt_and_reopen() {
    use crate::tasks::CancellationToken;
    let fixture = Fixture::new();
    let mut journal = terminal_rules_journal();
    let sequences = [9_007_199_254_740_993_u64, u64::MAX - 1];
    let entries = journal["entries"].as_array_mut().unwrap();
    entries.truncate(2);
    for (entry, sequence) in entries.iter_mut().zip(sequences) {
        entry["sequence"] = json!(sequence);
    }
    journal["next_sequence"] = json!(u64::MAX);
    fixture.write("state/operation-journals.json", &journal);
    let before = snapshot(&fixture.baseline);
    let (_, key) = crate::performance::rules::tests::signed_cache("2001-01-01T00:00:00Z");
    let (root, service) = import_service();
    let rules = rules_import_owner(root.path(), &service, key.clone());
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    assert!(preview.rules_import_available);
    let request = model::RulesImportRequest {
        fingerprint: preview.fingerprint,
        rules_import_id: preview.rules_import_id,
    };
    let pin = service
        .directories()
        .library()
        .admit_application_root()
        .unwrap();
    let response = previews
        .prepare_rules(&request.fingerprint)
        .unwrap()
        .commit(&rules, &pin, &request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        response
            .receipt
            .refresh_history
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        sequences
    );
    let encoded = serde_json::to_value(&response).unwrap();
    assert_eq!(
        encoded["receipt"]["refresh_history"][0]["sequence"],
        "9007199254740993"
    );
    assert_eq!(
        encoded["receipt"]["refresh_history"][1]["sequence"],
        "18446744073709551614"
    );
    drop(rules);
    let reopened = rules_import_owner(root.path(), &service, key);
    assert_eq!(
        rules_status(&reopened, &request.rules_import_id)
            .unwrap()
            .receipt,
        Some(response.receipt.clone())
    );
    let replay = previews
        .prepare_rules(&request.fingerprint)
        .unwrap()
        .commit(&reopened, &pin, &request, &CancellationToken::new())
        .await
        .unwrap();
    assert!(replay.already_imported);
    assert_eq!(replay.receipt, response.receipt);
    assert_eq!(fixture.record("state/operation-journals.json"), journal);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[test]
fn rules_import_strict_source_schema_keeps_unknown_and_nonterminal_obligations_blocking() {
    for invalid in [
        "running",
        "unknown",
        "target",
        "intent",
        "step",
        "sequence",
        "timestamp",
        "cache-envelope",
        "cache-signature",
        "alternate-file",
    ] {
        let fixture = Fixture::new();
        seed_rules_source(&fixture);
        let mut journal = terminal_rules_journal();
        let entry = &mut journal["entries"][0];
        match invalid {
            "running" => {
                entry["status"] = json!("Running");
                entry["outcome"] = Value::Null;
            }
            "unknown" => entry["command"] = json!("Unknown"),
            "target" => entry["targets"][0]["id"] = json!("other"),
            "intent" => entry["intent"]["unexpected"] = json!(true),
            "step" => entry["completed_steps"][0]["result"] = json!("Planned"),
            "sequence" => entry["sequence"] = json!(0),
            "timestamp" => entry["created_at"] = json!("2001-01-01T00:00:00Z"),
            "cache-envelope" | "cache-signature" => {
                let mut cache = fixture.record("performance/rules-cache.json");
                cache[if invalid == "cache-envelope" {
                    "schema_version"
                } else {
                    "signature"
                }] = json!(0);
                fixture.write("performance/rules-cache.json", &cache);
            }
            "alternate-file" => fixture.write("performance/other.json", &json!({})),
            _ => unreachable!(),
        }
        fixture.write("state/operation-journals.json", &journal);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(!inventory.preview().rules_import_available, "{invalid}");
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{invalid}"
        );
        assert!(
            inventory.prepare_rules(inventory.fingerprint()).is_err(),
            "{invalid}"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn rules_import_rejects_duplicate_changed_target_fields_from_original_bytes() {
    let fixture = Fixture::new();
    seed_rules_source(&fixture);
    let journal = terminal_rules_journal();
    let bytes = serde_json::to_string(&journal).unwrap();
    let target = "\"changed_target\":{\"id\":\"performance_rules_cache\",";
    assert!(bytes.contains(target));
    let bytes = bytes.replacen(
        target,
        "\"changed_target\":{\"id\":\"other\",\"id\":\"performance_rules_cache\",",
        1,
    );
    fs::create_dir_all(fixture.baseline.join("state")).unwrap();
    fs::write(
        fixture.baseline.join("state/operation-journals.json"),
        bytes,
    )
    .unwrap();
    let before = snapshot(&fixture.baseline);
    let inventory = Arc::new(fixture.capture());
    assert!(!inventory.preview().rules_import_available);
    assert!(inventory.prepare_rules(inventory.fingerprint()).is_err());
    assert_eq!(snapshot(&fixture.baseline), before);
}

fn historical_commands(service: &crate::instances::create::InstanceService) -> Vec<Value> {
    service
        .registry()
        .storage()
        .read(|db| -> Result<_, crate::storage::StorageError> {
            let mut query = db.prepare(
                "SELECT payload FROM performance_commands WHERE state='historical' ORDER BY rowid",
            )?;
            let bytes = query
                .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(bytes
                .into_iter()
                .map(|bytes| serde_json::from_slice(&bytes).unwrap())
                .collect())
        })
        .unwrap()
}

fn performance_reader(
    root: &Path,
    instances: &crate::instances::create::InstanceService,
) -> crate::performance::PerformanceService {
    use crate::{
        content::catalog::ContentService,
        network::{ClientConfig, ProviderClient},
        storage::MetadataStore,
        tasks::TaskOwner,
    };
    crate::performance::PerformanceService::new(
        Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap()),
        instances.directories().clone(),
        TaskOwner::new(8).unwrap(),
        Arc::new(
            ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap()).unwrap(),
        ),
        crate::performance::public_transfer_resolver(),
    )
    .unwrap()
}

#[test]
fn terminal_performance_import_rejects_incoherent_unknown_and_nonterminal_records() {
    for invalid in [
        "pending",
        "terminal-intent",
        "status",
        "target",
        "sequence",
        "next-sequence",
        "time",
        "proof",
        "unknown",
        "instance-identity",
        "error",
    ] {
        let fixture = Fixture::new();
        let mut journal = terminal_performance_journal();
        match invalid {
            "pending" => {
                journal["entries"][0]["intent"]["phase"]["phase"] = json!("effect_started")
            }
            "terminal-intent" => {
                journal["entries"][0]["intent"]["phase"]["phase"] = json!("terminal_intent")
            }
            "status" => journal["entries"][0]["status"] = json!("Running"),
            "target" => journal["entries"][0]["targets"][0]["id"] = json!(SECOND),
            "sequence" => {
                journal["entries"][0]["sequence"] = journal["entries"][1]["sequence"].clone()
            }
            "next-sequence" => journal["next_sequence"] = json!(6),
            "time" => journal["entries"][0]["intent"]["updated_at"] = json!("2024-02-29T12:35:56Z"),
            "proof" => {
                journal["entries"][0]["intent"]["phase"]["terminal"]["changed_target"] = json!(true)
            }
            "unknown" => journal["entries"][0]["retained_effect"] = json!({"must":"survive"}),
            "instance-identity" => {
                journal["entries"][0]["intent"]["intent"]["instance_id"] = json!("invalid");
                journal["entries"][0]["targets"][0]["id"] = json!("invalid");
            }
            "error" => {
                journal["entries"][3]["intent"]["phase"]["terminal"]["error"] =
                    json!("/private/sensitive-provider-path")
            }
            _ => unreachable!(),
        }
        fixture.write("state/operation-journals.json", &journal);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{invalid}"
        );
        assert!(
            inventory
                .prepare_instance(&inventory.preview().fingerprint, FIRST)
                .is_err(),
            "{invalid}"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
    for journal in [
        json!({"schema":"axial.state.operation_journals.v10", "entries":[]}),
        json!({"schema":"axial.state.operation_journals.v10", "next_sequence":0, "entries":[]}),
        json!({"schema":"axial.state.operation_journals.v10", "next_sequence":1, "entries":[], "unknown":true}),
    ] {
        let fixture = Fixture::new();
        fixture.write("state/operation-journals.json", &journal);
        let inventory = fixture.capture();
        assert!(!inventory.preview().instances[0].ordinary_import_available);
    }
}

#[tokio::test]
async fn terminal_performance_import_preserves_source_bound_evidence_and_exact_completed_replay() {
    let mut source_ids = Vec::new();
    // Keep both roots alive: filesystems may reuse deleted directory identities.
    let fixtures = [Fixture::new(), Fixture::new()];
    for fixture in &fixtures {
        let journal = terminal_performance_journal();
        fixture.write("state/operation-journals.json", &journal);
        let before = snapshot(&fixture.baseline);
        let (root, service) = import_service();
        let imported = service
            .import_instance(prepare_first(fixture))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let records = historical_commands(&service);
        assert_eq!(records.len(), 6);
        let reader = performance_reader(root.path(), &service);
        assert_eq!(reader.pending_count().unwrap(), 0);
        assert!(!reader.has_unsettled_effects());
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record["instance_id"], imported.id.as_str());
            assert_eq!(record["evidence"]["sequence"], index + 1);
            let original = journal["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["sequence"] == index + 1)
                .unwrap();
            assert_eq!(record["evidence"]["intent"], original["intent"]["intent"]);
            assert_eq!(
                record["evidence"]["terminal"],
                original["intent"]["phase"]["terminal"]
            );
            let status = reader
                .operation(record["id"].as_str().unwrap())
                .unwrap()
                .unwrap();
            assert!(status.history.is_some());
            assert_eq!(status.created_at, "2024-02-29T12:34:56.000Z");
            assert!(matches!(
                status.state.as_str(),
                "complete" | "failed" | "interrupted"
            ));
        }
        assert_eq!(
            reader
                .instance_operation(&imported.id)
                .unwrap()
                .unwrap()
                .history
                .unwrap()
                .sequence,
            6
        );
        source_ids.push(records[0]["id"].clone());
        let repeated = service
            .import_instance(prepare_first(fixture))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated.id, imported.id);
        assert_eq!(historical_commands(&service), records);
        drop(reader);
        service
            .registry()
            .storage()
            .transaction(|tx| -> Result<_, crate::storage::StorageError> {
                tx.execute(
                    "DELETE FROM performance_commands WHERE id=?1",
                    [records[0]["id"].as_str().unwrap()],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            service
                .import_instance(prepare_first(fixture))
                .unwrap()
                .join()
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(
            historical_commands(&service).len(),
            5,
            "completed replay never resurrects missing history"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
    assert_ne!(
        source_ids[0], source_ids[1],
        "identical content in different physical profiles has distinct history IDs"
    );
}

#[tokio::test]
async fn managed_import_preserves_real_rollback_history_and_completed_replay_keeps_user_changes() {
    use axial_performance::{ManagedRollbackOutcome, PerformanceManager, RollbackSnapshotTarget};
    let fixture = Fixture::new();
    let state = seed_managed_source(&fixture).await;
    let before = snapshot(&fixture.baseline);
    let (_root, service) = import_service();
    let imported = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(imported.settings.performance_mode, "managed");
    assert!(imported.settings.auto_optimize);
    let path = imported_path(&service, &imported.id);
    let original = snapshot(&fixture.baseline.join(format!("instances/{FIRST}")));
    let copied = snapshot(&path);
    assert_eq!(
        original.keys().collect::<Vec<_>>(),
        copied.keys().collect::<Vec<_>>()
    );
    for (relative, (bytes, inode, _)) in &original {
        assert_eq!(bytes, &copied[relative].0, "{}", relative.display());
        #[cfg(unix)]
        assert_ne!(*inode, copied[relative].1, "{}", relative.display());
        #[cfg(not(unix))]
        let _ = inode;
    }
    let copy = service.directories().admit(&imported.id).unwrap();
    let manager = Arc::new(PerformanceManager::new().unwrap());
    let (authority, identity) = manager
        .bind_admitted_instance(imported.id.as_str(), copy.directory().capability().clone())
        .unwrap();
    let effects = authority
        .bind_instance_effect_authority(&identity)
        .await
        .unwrap();
    let inspection = authority
        .recover_and_inspect(&identity, &effects)
        .await
        .unwrap();
    assert_eq!(inspection.state, Some(state));
    let absent = inspection
        .rollback_snapshots
        .iter()
        .find(|snapshot| snapshot.target == RollbackSnapshotTarget::ManagedStateAbsent)
        .unwrap();
    assert!(
        inspection
            .rollback_snapshots
            .iter()
            .any(|snapshot| snapshot.target == RollbackSnapshotTarget::ManagedComposition)
    );
    assert_eq!(
        authority
            .rollback_managed_snapshot(&identity, &effects, &absent.id)
            .await
            .unwrap(),
        ManagedRollbackOutcome::ManagedStateAbsent
    );
    assert!(!path.join("mods/.axial-lock.json").exists());
    assert!(!path.join("mods/current.jar").exists());
    assert_eq!(
        fs::read(path.join("mods/user-owned.jar")).unwrap(),
        fs::read(
            fixture
                .baseline
                .join(format!("instances/{FIRST}/mods/user-owned.jar"))
        )
        .unwrap()
    );
    drop(effects);
    drop(authority);
    drop(copy);
    let after_rollback = snapshot(&path);
    let repeated = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(snapshot(&path), after_rollback);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn provenance_import_preserves_exact_stale_metadata_without_fabricating_live_ownership() {
    let fixture = Fixture::new();
    let bytes = seed_content_source(&fixture);
    let before = snapshot(&fixture.baseline);
    let (_root, service) = import_service();
    let imported = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let admitted = service.directories().admit(&imported.id).unwrap();
    let path = imported_path(&service, &imported.id);
    let (manifest, live, raw) = crate::content::install::observe(admitted.directory()).unwrap();
    assert_eq!(raw.as_deref(), Some(bytes.as_slice()));
    assert_eq!(manifest.len(), 5);
    for entry in manifest.entries() {
        assert_eq!(
            live.contains(entry),
            matches!(entry.project_id(), "enabled" | "disabled")
        );
        assert_eq!(entry.installed_at(), "2024-02-29T12:34:56+02:00");
        assert!(entry.pack_installation().is_none());
    }
    assert_eq!(fs::read(path.join("axial.content.json")).unwrap(), bytes);
    drop(admitted);
    fs::write(
        path.join("axial.content.json"),
        b"user edited destination provenance",
    )
    .unwrap();
    fs::write(
        path.join("mods/enabled.jar"),
        b"user changed destination artifact",
    )
    .unwrap();
    let edited = snapshot(&path);
    let repeated = service
        .import_instance(prepare_first(&fixture))
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(snapshot(&path), edited);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn managed_import_keeps_rules_journals_and_unsettled_payloads_blocking() {
    for obligation in [
        "rules",
        "terminal-journal",
        "nonterminal-journal",
        "candidate",
        "unknown",
        "artifact",
    ] {
        let fixture = Fixture::new();
        seed_managed_source(&fixture).await;
        match obligation {
            "rules" => fixture.write("performance/rules-cache.json", &json!({"schema_version":1})),
            "terminal-journal" | "nonterminal-journal" => fixture.write(
                "state/operation-journals.json",
                &json!({"schema":"axial.state.operation_journals.v10", "entries":[{
                    "instance_id":FIRST, "intent":{"kind":"performance"},
                    "lifecycle":{"phase": if obligation == "terminal-journal" {"terminal"} else {"running"}}
                }]}),
            ),
            "candidate" | "unknown" => {
                let leaf = if obligation == "candidate" {"rollback/tmp/unsettled"} else {"unknown/retained"};
                let path = fixture.baseline.join(format!("instances/{FIRST}/mods/.axial-performance/{leaf}"));
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"retained obligation").unwrap();
            }
            "artifact" => fs::write(fixture.baseline.join(format!("instances/{FIRST}/mods/current.jar")), b"changed artifact").unwrap(),
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{obligation}"
        );
        assert!(
            inventory
                .prepare_instance(&inventory.preview().fingerprint, FIRST)
                .is_err(),
            "{obligation}"
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn provenance_import_does_not_waive_other_reserved_records_or_source_extensions() {
    for unsupported in ["replacement-field", "nested", "scratch"] {
        let fixture = Fixture::new();
        seed_content_source(&fixture);
        match unsupported {
            "replacement-field" => {
                let path = format!("instances/{FIRST}/axial.content.json");
                let mut manifest = fixture.record(&path);
                manifest["entries"][4]["pack_installation"] = Value::Null;
                fixture.write(&path, &manifest);
            }
            "nested" => fixture.write(
                &format!("instances/{FIRST}/mods/axial.content.json"),
                &json!({"schema_version":3,"entries":[]}),
            ),
            "scratch" => {
                fs::create_dir(
                    fixture
                        .baseline
                        .join(format!("instances/{FIRST}/.axial-content")),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        assert!(
            !inventory.preview().instances[0].ordinary_import_available,
            "{unsupported}"
        );
        assert!(
            inventory
                .prepare_instance(&inventory.preview().fingerprint, FIRST)
                .is_err()
        );
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

fn reopen_import_service(
    root: &Path,
    library_id: crate::library::LibraryId,
) -> crate::instances::create::InstanceService {
    use crate::{
        instances::{
            create::InstanceService,
            directory::{InstanceDirectories, Registry},
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        storage::MetadataStore,
        tasks::{Exclusions, TaskOwner},
    };
    let library = match LibraryLifecycle::open_with_id(root, library_id) {
        LibraryOpenOutcome::Ready(library) => library,
        other => panic!("isolated restarted import library: {other:?}"),
    };
    let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
    InstanceService::new(
        InstanceDirectories::new(Registry::new(storage), library, Exclusions::new()),
        TaskOwner::new(16).unwrap(),
    )
}

#[tokio::test]
async fn instance_import_last_selection_maps_once_and_preserves_destination_choices() {
    use crate::instances::model::InstanceResult;

    for existing_selection in [false, true] {
        let fixture = Fixture::new();
        fixture.two_instances();
        let mut registry = fixture.record("instances.json");
        registry["last_instance_id"] = json!(SECOND);
        registry["instances"][1]["last_played_at"] = json!("2024-02-29T12:34:56Z");
        fixture.write("instances.json", &registry);
        let before = snapshot(&fixture.baseline);
        let inventory = Arc::new(fixture.capture());
        let fingerprint = inventory.preview().fingerprint;
        let (root, service) = import_service();
        let first = service
            .import_instance(inventory.prepare_instance(&fingerprint, FIRST).unwrap())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(service.registry().last_instance_id().unwrap(), None);
        if existing_selection {
            service.registry().select(&first.id).unwrap();
        }
        let selected = service
            .import_instance(inventory.prepare_instance(&fingerprint, SECOND).unwrap())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let expected = if existing_selection {
            &first.id
        } else {
            &selected.id
        };
        assert_eq!(
            service.registry().last_instance_id().unwrap().as_ref(),
            Some(expected)
        );
        assert_eq!(selected.last_played_at, "2024-02-29T12:34:56Z");
        assert_eq!(selected.created_at, registry["instances"][1]["created_at"]);
        assert_eq!(
            selected.revision, 2,
            "selection must not fabricate a launch revision"
        );
        let selected_record = service.registry().get_live(&selected.id).unwrap();
        let copied = snapshot(&imported_path(&service, &selected.id));
        let library_id = service
            .directories()
            .library()
            .admit()
            .unwrap()
            .library_id();
        drop(service);
        let service = reopen_import_service(root.path(), library_id);
        assert_eq!(
            service.registry().last_instance_id().unwrap().as_ref(),
            Some(expected)
        );

        service.registry().select(&first.id).unwrap();
        for cleared_selection in [false, true] {
            if cleared_selection {
                service
                    .registry()
                    .storage()
                    .transaction(|tx| -> InstanceResult<()> {
                        assert_eq!(
                            tx.execute(
                                "UPDATE instance_selection SET instance_id=NULL WHERE singleton=1",
                                []
                            )?,
                            1
                        );
                        Ok(())
                    })
                    .unwrap();
            }
            let inventory = Arc::new(fixture.capture());
            let repeated = service
                .import_instance(
                    inventory
                        .prepare_instance(&inventory.preview().fingerprint, SECOND)
                        .unwrap(),
                )
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(repeated.id, selected.id);
            assert_eq!(
                service.registry().last_instance_id().unwrap(),
                (!cleared_selection).then(|| first.id.clone())
            );
            assert_eq!(
                service.registry().get_live(&selected.id).unwrap(),
                selected_record
            );
            assert_eq!(snapshot(&imported_path(&service, &selected.id)), copied);
        }
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn instance_import_last_selection_rolls_back_and_recovers_with_publication() {
    use crate::instances::model::{InstanceError, InstanceResult};

    for failure in [
        "ignored-selection",
        "ignored-completion",
        "failed-completion",
        "missing-selection",
    ] {
        let fixture = Fixture::new();
        let before = snapshot(&fixture.baseline);
        let (root, service) = import_service();
        let prepared = prepare_first(&fixture);
        let id = prepared.instance().id.clone();
        let (inject, repair) = match failure {
            "ignored-selection" => (
                "CREATE TRIGGER refuse_selection BEFORE UPDATE ON instance_selection BEGIN SELECT RAISE(IGNORE); END;",
                "DROP TRIGGER refuse_selection",
            ),
            "ignored-completion" => (
                "CREATE TRIGGER refuse_completion BEFORE UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(IGNORE); END;",
                "DROP TRIGGER refuse_completion",
            ),
            "failed-completion" => (
                "CREATE TRIGGER refuse_completion BEFORE UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(ABORT, 'injected completion failure'); END;",
                "DROP TRIGGER refuse_completion",
            ),
            _ => (
                "DELETE FROM instance_selection WHERE singleton=1",
                "INSERT INTO instance_selection(singleton,instance_id) VALUES(1,NULL)",
            ),
        };
        service
            .registry()
            .storage()
            .transaction(|tx| -> InstanceResult<()> {
                tx.execute_batch(inject)?;
                Ok(())
            })
            .unwrap();
        let result = service
            .import_instance(prepared)
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(
            matches!(
                result,
                Err(InstanceError::Conflict | InstanceError::Storage(_))
            ),
            "{failure}: {result:?}"
        );
        assert!(service.registry().list().unwrap().is_empty(), "{failure}");
        if failure == "missing-selection" {
            assert!(service.registry().last_instance_id().is_err());
        } else {
            assert_eq!(service.registry().last_instance_id().unwrap(), None);
        }
        assert_eq!(service.pending().unwrap()[0].instance_id, id);
        let path = imported_path(&service, &id);
        let copied = snapshot(&path);
        assert_eq!(
            fs::read(path.join("options.txt")).unwrap(),
            fs::read(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/options.txt"))
            )
            .unwrap()
        );
        service
            .registry()
            .storage()
            .transaction(|tx| -> InstanceResult<()> {
                tx.execute_batch(repair)?;
                Ok(())
            })
            .unwrap();
        let library_id = service
            .directories()
            .library()
            .admit()
            .unwrap()
            .library_id();
        drop(service);
        let service = reopen_import_service(root.path(), library_id);
        assert!(
            service.recover_pending().await.is_err(),
            "source-free recovery must not publish {failure}"
        );
        assert_eq!(service.registry().last_instance_id().unwrap(), None);
        let recovered = service
            .import_instance(prepare_first(&fixture))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.id, id);
        assert_eq!(service.registry().last_instance_id().unwrap(), Some(id));
        assert!(recovered.last_played_at.is_empty());
        assert_eq!(recovered.revision, 2);
        assert!(service.pending().unwrap().is_empty());
        assert_eq!(snapshot(&path), copied);
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn managed_provenance_import_recovers_real_publication_only_with_exact_copied_evidence() {
    use crate::instances::model::{InstanceError, InstanceResult};
    for change_destination in [false, true] {
        let fixture = Fixture::new();
        seed_managed_source(&fixture).await;
        let manifest = seed_content_source(&fixture);
        fixture.write(
            "state/operation-journals.json",
            &terminal_performance_journal(),
        );
        let before = snapshot(&fixture.baseline);
        let (root, service) = import_service();
        let prepared = prepare_first(&fixture);
        let id = prepared.instance().id.clone();
        service.registry().storage().transaction(|tx| -> InstanceResult<()> {
            tx.execute_batch("CREATE TRIGGER reject_import_commit BEFORE UPDATE OF phase ON instance_creations WHEN NEW.phase='complete' BEGIN SELECT RAISE(ABORT, 'injected import commit failure'); END;")?;
            Ok(())
        }).unwrap();
        assert!(matches!(
            service
                .import_instance(prepared)
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(InstanceError::Storage(_))
        ));
        assert!(service.registry().list().unwrap().is_empty());
        assert!(historical_commands(&service).is_empty());
        assert_eq!(service.pending().unwrap()[0].instance_id, id);
        let path = imported_path(&service, &id);
        assert_eq!(fs::read(path.join("axial.content.json")).unwrap(), manifest);
        service
            .registry()
            .storage()
            .transaction(|tx| -> InstanceResult<()> {
                tx.execute_batch("DROP TRIGGER reject_import_commit")?;
                Ok(())
            })
            .unwrap();
        let library_id = service
            .directories()
            .library()
            .admit()
            .unwrap()
            .library_id();
        drop(service);
        let service = reopen_import_service(root.path(), library_id);
        assert!(
            service.recover_pending().await.is_err(),
            "source-free recovery cannot adopt the copy"
        );
        if change_destination {
            fs::write(
                path.join("mods/current.jar"),
                b"independent destination edit",
            )
            .unwrap();
        }
        let copied_before_retry = snapshot(&path);
        let result = service
            .import_instance(prepare_first(&fixture))
            .unwrap()
            .join()
            .await
            .unwrap();
        if change_destination {
            assert!(result.is_err());
            assert!(service.registry().list().unwrap().is_empty());
            assert_eq!(service.pending().unwrap()[0].instance_id, id);
            assert_eq!(snapshot(&path), copied_before_retry);
            assert!(historical_commands(&service).is_empty());
        } else {
            assert_eq!(result.unwrap().id, id);
            assert_eq!(service.registry().list().unwrap().len(), 1);
            assert!(service.pending().unwrap().is_empty());
            assert_eq!(historical_commands(&service).len(), 6);
            let reader = performance_reader(root.path(), &service);
            assert_eq!(
                reader
                    .instance_operation(&id)
                    .unwrap()
                    .unwrap()
                    .history
                    .unwrap()
                    .sequence,
                6
            );
            assert_eq!(reader.pending_count().unwrap(), 0);
            drop(reader);
            let repeated = service
                .import_instance(prepare_first(&fixture))
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(repeated.id, id);
        }
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[tokio::test]
async fn managed_provenance_import_rejects_changed_source_and_cancelled_capture_before_copy() {
    use crate::instances::model::InstanceError;
    for cancel_capture in [false, true] {
        let fixture = Fixture::new();
        seed_managed_source(&fixture).await;
        seed_content_source(&fixture);
        let cancelled = Arc::new(AtomicBool::new(false));
        let inventory = Arc::new(
            Inventory::capture_controlled(
                &fixture.source,
                &BTreeMap::new(),
                CaptureLimits::default(),
                cancelled.clone(),
            )
            .unwrap(),
        );
        let prepared = inventory
            .prepare_instance(&inventory.preview().fingerprint, FIRST)
            .unwrap();
        if cancel_capture {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        } else {
            fs::write(
                fixture
                    .baseline
                    .join(format!("instances/{FIRST}/axial.content.json")),
                b"changed after preparation",
            )
            .unwrap();
        }
        let before = snapshot(&fixture.baseline);
        let (_root, service) = import_service();
        let result = service.import_instance(prepared);
        assert!(matches!(
            result,
            Err(InstanceError::Conflict | InstanceError::Cancelled)
        ));
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.pending().unwrap().is_empty());
        assert_eq!(snapshot(&fixture.baseline), before);
    }
}

#[test]
fn managed_import_admits_many_settled_payloads_without_retaining_effect_owners() {
    let fixture = Fixture::new();
    let mut registry = fixture.record("instances.json");
    let first = registry["instances"][0].clone();
    let mut instances = Vec::new();
    // The leaf effect owner has 256 slots. Immutable preview witnesses must not
    // consume one for every instance in the source-wide (4096 row) budget.
    for index in 1..=257 {
        let id = format!("{index:016x}");
        let mut instance = first.clone();
        instance["id"] = json!(id);
        instance["name"] = json!(format!("Managed {index}"));
        instance["performance_mode"] = json!("managed");
        instance["auto_optimize"] = json!(true);
        instances.push(instance);
        fs::create_dir_all(
            fixture
                .baseline
                .join(format!("instances/{id}/mods/.axial-performance")),
        )
        .unwrap();
    }
    registry["instances"] = json!(instances);
    fixture.write("instances.json", &registry);
    let before = snapshot(&fixture.baseline);
    let inventory = Arc::new(fixture.capture());
    assert_eq!(inventory.preview().instances.len(), 257);
    assert!(
        inventory
            .preview()
            .instances
            .iter()
            .all(|instance| instance.ordinary_import_available)
    );
    let prepared = inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap();
    assert_eq!(prepared.instance().settings.performance_mode, "managed");
    assert!(prepared.instance().settings.auto_optimize);
    assert_eq!(snapshot(&fixture.baseline), before);
}

#[tokio::test]
async fn instance_import_publishes_independent_complete_payload_and_repeated_import_preserves_user_changes()
 {
    let fixture = Fixture::new();
    let source = fixture.baseline.join("instances").join(FIRST);
    fs::create_dir(source.join("screenshots")).unwrap();
    fs::create_dir(source.join("logs")).unwrap();
    fs::create_dir(source.join("empty-user-directory")).unwrap();
    fs::write(
        source.join("screenshots/retained.png"),
        b"original screenshot",
    )
    .unwrap();
    fs::write(source.join("logs/latest.log"), b"original log").unwrap();
    fs::write(source.join("unrecognized.user-data"), b"must be preserved").unwrap();
    let before = snapshot(&fixture.baseline);
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    let (_root, service) = import_service();
    let imported = service
        .import_instance(
            previews
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let path = imported_path(&service, &imported.id);
    let source_payload = snapshot(&source);
    let destination_payload = snapshot(&path);
    assert_eq!(
        source_payload.keys().collect::<Vec<_>>(),
        destination_payload.keys().collect::<Vec<_>>()
    );
    for (relative, (bytes, inode, links)) in &source_payload {
        let copied = &destination_payload[relative];
        assert_eq!(bytes, &copied.0, "{}", relative.display());
        #[cfg(unix)]
        {
            assert_ne!(
                *inode,
                copied.1,
                "{} is independently copied",
                relative.display()
            );
            assert_eq!(*links, copied.2);
        }
        #[cfg(not(unix))]
        let _ = (inode, links);
    }
    assert_eq!(before, snapshot(&fixture.baseline));
    let record = service.registry().get_live(&imported.id).unwrap();
    assert_eq!(record.instance.settings.performance_mode, "vanilla");
    assert_eq!(
        record.instance.settings.extra_jvm_args,
        "-Dfixture.marker=synthetic"
    );
    fs::write(path.join("options.txt"), b"changed only in replacement").unwrap();
    fs::write(path.join("new-user-file.keep"), b"new replacement work").unwrap();
    let after_user_edit = snapshot(&path);
    let repeated_preview = previews.admit(fixture.capture()).unwrap();
    let repeated = service
        .import_instance(
            previews
                .prepare_instance(&repeated_preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.id, imported.id);
    assert_eq!(service.registry().list().unwrap().len(), 1);
    assert_eq!(after_user_edit, snapshot(&path));
    assert_eq!(before, snapshot(&fixture.baseline));
    assert!(!previews.current().unwrap().cutover_available);
}

#[tokio::test]
async fn accepted_copy_survives_forget_and_late_capture_cancellation() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.baseline);
    let cancelled = Arc::new(AtomicBool::new(false));
    let inventory = Inventory::capture_controlled(
        &fixture.source,
        &BTreeMap::new(),
        CaptureLimits::default(),
        cancelled.clone(),
    )
    .unwrap();
    let previews = ImportPreviews::new();
    let preview = previews.admit(inventory).unwrap();
    let prepared = previews
        .prepare_instance(&preview.fingerprint, FIRST)
        .unwrap();
    let (_root, service) = import_service();
    let work = service.import_instance(prepared.clone()).unwrap();
    previews.forget().unwrap();
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    // Deterministic even if the task finished before the cancellation request.
    prepared.revalidate().unwrap();
    let imported = work.join().await.unwrap().unwrap();
    let repeated = service
        .import_instance(prepared)
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(imported.id, repeated.id);
    assert_eq!(service.registry().list().unwrap().len(), 1);
    assert_eq!(
        fs::read(imported_path(&service, &imported.id).join("options.txt")).unwrap(),
        fs::read(
            fixture
                .baseline
                .join("instances")
                .join(FIRST)
                .join("options.txt")
        )
        .unwrap(),
    );
    assert!(matches!(previews.current(), Err(ImportError::NoSource)));
    assert_eq!(before, snapshot(&fixture.baseline));
}

#[tokio::test]
async fn changed_source_cannot_overwrite_or_duplicate_an_existing_import() {
    let fixture = Fixture::new();
    let previews = ImportPreviews::new();
    let preview = previews.admit(fixture.capture()).unwrap();
    let (_root, service) = import_service();
    let imported = service
        .import_instance(
            previews
                .prepare_instance(&preview.fingerprint, FIRST)
                .unwrap(),
        )
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let path = imported_path(&service, &imported.id);
    let destination_before = snapshot(&path);
    fs::write(
        fixture
            .baseline
            .join(format!("instances/{FIRST}/options.txt")),
        b"new source content",
    )
    .unwrap();
    let source_before = snapshot(&fixture.baseline);
    assert!(matches!(
        previews.prepare_instance(&preview.fingerprint, FIRST),
        Err(ImportError::SourceChanged)
    ));
    let revised = previews.admit(fixture.capture()).unwrap();
    assert_ne!(revised.fingerprint, preview.fingerprint);
    assert!(matches!(
        service.import_instance(
            previews
                .prepare_instance(&revised.fingerprint, FIRST)
                .unwrap()
        ),
        Err(crate::instances::model::InstanceError::Conflict)
    ));
    assert_eq!(service.registry().list().unwrap().len(), 1);
    assert_eq!(destination_before, snapshot(&path));
    assert_eq!(source_before, snapshot(&fixture.baseline));
}

#[tokio::test]
async fn instance_import_never_adopts_or_removes_an_unknown_destination() {
    let fixture = Fixture::new();
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap();
    let id = prepared.instance().id.clone();
    let source_before = snapshot(&fixture.baseline);
    let (_root, service) = import_service();
    let path = imported_path(&service, &id);
    fs::create_dir_all(&path).unwrap();
    fs::write(
        path.join("foreign.keep"),
        b"unknown destination must survive",
    )
    .unwrap();
    let before = snapshot(&path);
    assert!(
        service
            .import_instance(prepared)
            .unwrap()
            .join()
            .await
            .unwrap()
            .is_err()
    );
    assert!(service.registry().list().unwrap().is_empty());
    assert_eq!(before, snapshot(&path));
    assert_eq!(source_before, snapshot(&fixture.baseline));
}

#[tokio::test]
async fn repeated_import_refuses_a_replaced_destination_without_touching_either_tree() {
    let fixture = Fixture::new();
    let inventory = Arc::new(fixture.capture());
    let prepared = inventory
        .prepare_instance(&inventory.preview().fingerprint, FIRST)
        .unwrap();
    let (_root, service) = import_service();
    let imported = service
        .import_instance(prepared.clone())
        .unwrap()
        .join()
        .await
        .unwrap()
        .unwrap();
    let path = imported_path(&service, &imported.id);
    let preserved = path.with_extension("preserved");
    fs::rename(&path, &preserved).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("foreign.keep"), b"replacement is not owned").unwrap();
    let source_before = snapshot(&fixture.baseline);
    let before = snapshot(&path);
    let preserved_before = snapshot(&preserved);
    assert!(
        service
            .import_instance(prepared)
            .unwrap()
            .join()
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(before, snapshot(&path));
    assert_eq!(preserved_before, snapshot(&preserved));
    assert_eq!(source_before, snapshot(&fixture.baseline));
}
