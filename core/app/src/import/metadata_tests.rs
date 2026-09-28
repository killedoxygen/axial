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
        .migrate(&[METADATA_IMPORT_IDENTITIES_MIGRATION])
        .unwrap();
    let receipt = metadata_status(&old_settings, &old_id)
        .unwrap()
        .receipt
        .unwrap();
    assert_eq!(receipt.imported_offline_account_count, 2);
    assert_eq!(receipt.imported_microsoft_account_count, 0);
    assert_eq!(receipt.account_id_mapping, None);
    assert_eq!(receipt.settings_revision, 3);

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
