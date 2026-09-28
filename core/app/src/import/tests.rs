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
        Inventory::capture(&self.source, &BTreeMap::new()).unwrap()
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

fn terminal_performance_journal() -> Value {
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
        "source-instance",
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
            "source-instance" => {
                journal["entries"][0]["intent"]["intent"]["instance_id"] = json!(SECOND);
                journal["entries"][0]["targets"][0]["id"] = json!(SECOND);
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
