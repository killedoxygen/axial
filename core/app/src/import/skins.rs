//! Read-only predecessor wardrobe conversion. Publication and the completed
//! receipt belong to the saved-skin library, not to an import journal.

use std::{collections::BTreeMap, sync::Arc};

use serde::Deserialize;

use super::{
    ImportError, ImportResult, Inventory,
    model::{SkinImportRequest, SkinImportResponse, SkinImportStatus},
};
use crate::{
    media::SKIN_PNG_MAX_BYTES,
    skins::{
        library::{
            PreparedSkinRecord, SavedSkinLibrary, SavedSkinRecord, SkinLibraryError,
            prepare_import_record, skin_import_id, validate_texture_key,
        },
        store::{MAX_SAVED_SKINS, MAX_TOTAL_PNG_BYTES},
    },
    tasks::CancellationToken,
};

const INDEX: &str = "profile/skins/index.json";
const PREFIX: &str = "profile/skins/";

#[derive(Debug, thiserror::Error)]
pub enum SkinImportError {
    #[error(transparent)]
    Source(#[from] ImportError),
    #[error(transparent)]
    Library(#[from] SkinLibraryError),
}

/// Only an admitted source inventory creates this immutable input. Forgetting
/// or replacing a preview cannot revoke already accepted work's source pin.
#[derive(Clone)]
pub struct PreparedSkinImport {
    inventory: Arc<Inventory>,
    source_id: String,
    import_id: String,
    records: Vec<PreparedSkinRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySkinIndex {
    schema: String,
    schema_version: u32,
    skins: Vec<SavedSkinRecord>,
}

impl Inventory {
    pub(super) fn skin_import_id(&self) -> ImportResult<String> {
        skin_import_id(&self.source_identity()?, self.fingerprint())
            .map_err(|_| ImportError::InvalidData)
    }

    pub(super) fn skin_import_available(&self) -> bool {
        self.skin_inputs().is_ok()
    }

    pub(super) fn prepare_skins(
        self: &Arc<Self>,
        fingerprint: &str,
    ) -> ImportResult<PreparedSkinImport> {
        self.revalidate()?;
        if fingerprint != self.fingerprint() {
            return Err(ImportError::SourceChanged);
        }
        let records = self.skin_inputs()?;
        let source_id = self.source_identity()?;
        let import_id = self.skin_import_id()?;
        self.revalidate()?;
        Ok(PreparedSkinImport {
            inventory: Arc::clone(self),
            source_id,
            import_id,
            records,
        })
    }

    fn skin_inputs(&self) -> ImportResult<Vec<PreparedSkinRecord>> {
        // Unknown directories, links, pending files and unlisted PNGs remain
        // retained blockers rather than being silently dropped from a batch.
        if self
            .directory_names()
            .any(|name| name.starts_with(PREFIX) && name != "profile/skins/files")
            || self.obligations().iter().any(|record| {
                record.source_record.starts_with(PREFIX)
                    && record.blocker == super::ImportBlocker::UnsafeFile
            })
        {
            return Err(ImportError::InvalidData);
        }
        let index: LegacySkinIndex = serde_json::from_slice(&self.record_bytes(INDEX)?)
            .map_err(|_| ImportError::InvalidData)?;
        if index.schema != "axial.skins.saved" || index.schema_version != 3 {
            return Err(ImportError::InvalidData);
        }
        if index.skins.len() > MAX_SAVED_SKINS {
            return Err(ImportError::LimitExceeded);
        }
        let mut files: BTreeMap<_, _> = self
            .file_manifests()
            .filter(|file| file.relative.starts_with(PREFIX) && file.relative != INDEX)
            .map(|file| (file.relative.as_str(), file))
            .collect();
        if files.len() != index.skins.len() {
            return Err(ImportError::InvalidData);
        }
        let mut total = 0_u64;
        let mut records = Vec::with_capacity(index.skins.len());
        for record in index.skins {
            self.check_cancelled()?;
            let relative = format!("profile/skins/files/{}.png", record.texture_key);
            let file = files
                .remove(relative.as_str())
                .ok_or(ImportError::InvalidData)?;
            if file.size == 0 || file.size > SKIN_PNG_MAX_BYTES as u64 {
                return Err(ImportError::LimitExceeded);
            }
            if file.size != record.byte_size as u64 || file.sha256 != record.texture_key {
                return Err(ImportError::InvalidData);
            }
            total = total
                .checked_add(file.size)
                .ok_or(ImportError::LimitExceeded)?;
            if total > MAX_TOTAL_PNG_BYTES {
                return Err(ImportError::LimitExceeded);
            }
            let png = self.record_bytes(&relative)?;
            records.push(prepare_import_record(record, png).map_err(|_| ImportError::InvalidData)?);
        }
        Ok(records)
    }
}

impl PreparedSkinImport {
    /// Run as retained blocking work. The actual destination library supplies
    /// root authority; callers cannot substitute an unrelated separation pin.
    pub fn commit(
        &self,
        library: &SavedSkinLibrary,
        request: &SkinImportRequest,
        cancel: &CancellationToken,
    ) -> Result<SkinImportResponse, SkinImportError> {
        if request.fingerprint != self.inventory.fingerprint()
            || request.skin_import_id != self.import_id
        {
            return Err(ImportError::SourceChanged.into());
        }
        check_cancel(cancel)?;
        let root = library.root_pin().directory().map_err(ImportError::Io)?;
        self.inventory.validate_destination_root(&root)?;
        check_cancel(cancel)?;
        let commit = library
            .import_batch(
                &self.source_id,
                self.inventory.fingerprint(),
                &self.records,
                cancel,
            )
            .map_err(|error| {
                if error == SkinLibraryError::Conflict && cancel.is_cancelled() {
                    SkinImportError::Source(ImportError::Cancelled)
                } else {
                    SkinImportError::Library(error)
                }
            })?;
        // Publication won. A late cancellation cannot turn its durable receipt
        // into a cancelled response or cause a replay to restore later edits.
        Ok(SkinImportResponse {
            receipt: commit.receipt,
            already_imported: commit.already_imported,
            cutover_available: false,
        })
    }
}

/// Completed-only reconciliation does not require a source volume or preview.
pub fn skin_status(
    library: &SavedSkinLibrary,
    import_id: &str,
) -> Result<SkinImportStatus, SkinImportError> {
    if !validate_texture_key(import_id).is_ok_and(|validated| validated == import_id) {
        return Err(ImportError::InvalidData.into());
    }
    Ok(SkinImportStatus {
        receipt: library.import_status(import_id)?,
        cutover_available: false,
    })
}

fn check_cancel(cancel: &CancellationToken) -> ImportResult<()> {
    if cancel.is_cancelled() {
        Err(ImportError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        accounts::directory::AccountDirectory,
        import::{
            ImportPreviews,
            tests::{Fixture, snapshot},
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        media::{SkinVariant, normalize_skin_png, texture_key},
        settings::SettingsStore,
        skins::{
            library::{SavedSkinDeleteResult, UpdateSavedSkinRequest},
            store::{IMPORT_MIGRATION, MIGRATION, SavedSkinStore},
        },
        storage::MetadataStore,
    };
    use serde_json::{Value, json};
    use std::{fs, path::Path};

    struct Destination {
        skins: SavedSkinLibrary,
        metadata: Arc<MetadataStore>,
        roots: LibraryLifecycle,
        root: tempfile::TempDir,
    }

    impl Destination {
        fn new() -> Self {
            let root =
                tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let roots = open_root(root.path());
            let metadata =
                Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
            let skins = library(Arc::clone(&metadata), &roots);
            Self {
                skins,
                metadata,
                roots,
                root,
            }
        }

        fn commit(
            &self,
            prepared: &PreparedSkinImport,
            request: &SkinImportRequest,
        ) -> Result<SkinImportResponse, SkinImportError> {
            prepared.commit(&self.skins, request, &CancellationToken::new())
        }
    }

    fn open_root(path: &Path) -> LibraryLifecycle {
        match LibraryLifecycle::open(path) {
            LibraryOpenOutcome::Ready(roots) => roots,
            other => panic!("isolated saved-skin destination: {other:?}"),
        }
    }

    fn library(metadata: Arc<MetadataStore>, roots: &LibraryLifecycle) -> SavedSkinLibrary {
        AccountDirectory::new(Arc::clone(&metadata)).unwrap();
        metadata.migrate(&[MIGRATION, IMPORT_MIGRATION]).unwrap();
        SavedSkinLibrary::new(
            SavedSkinStore::new(metadata),
            roots.admit_application_root().unwrap(),
        )
    }

    fn png(red: u8, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 64, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[red, 20, 30, 255].repeat(64 * height as usize))
                .unwrap();
        }
        bytes
    }

    pub(in crate::import) fn skin(red: u8) -> (SavedSkinRecord, Vec<u8>) {
        let bytes = normalize_skin_png(&png(red, 64)).unwrap().png_bytes;
        let record = SavedSkinRecord {
            texture_key: texture_key(&bytes),
            name: format!("Saved skin {red}"),
            variant: SkinVariant::Slim,
            source: "minecraft_username_skin".into(),
            cape_id: Some("retained-cape".into()),
            created_at: "2024-01-02T03:04:05.000+02:00".into(),
            updated_at: "2025-02-03T04:05:06.000Z".into(),
            applied_at: None,
            byte_size: bytes.len(),
        };
        (record, bytes)
    }

    fn write_index(source: &Fixture, value: &Value) {
        fs::create_dir_all(source.baseline.join("skins/files")).unwrap();
        fs::write(
            source.baseline.join("skins/index.json"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }

    pub(in crate::import) fn install(source: &Fixture, records: &[(SavedSkinRecord, Vec<u8>)]) {
        write_index(
            source,
            &json!({"schema":"axial.skins.saved","schema_version":3,
            "skins":records.iter().map(|(record, _)| record).collect::<Vec<_>>()}),
        );
        for (record, bytes) in records {
            fs::write(
                source
                    .baseline
                    .join(format!("skins/files/{}.png", record.texture_key)),
                bytes,
            )
            .unwrap();
        }
    }

    fn prepare(source: &Fixture) -> (PreparedSkinImport, SkinImportRequest) {
        let previews = ImportPreviews::new();
        let preview = previews.admit(source.capture()).unwrap();
        assert!(preview.skin_import_available);
        assert!(!preview.cutover_available);
        assert!(
            preview
                .blockers
                .contains(&super::super::ImportBlocker::SavedSkinsRequireConversion)
        );
        let prepared = previews.prepare_skins(&preview.fingerprint).unwrap();
        // This is the accepted immutable input, not a later lookup by preview.
        previews.forget().unwrap();
        (
            prepared,
            SkinImportRequest {
                skin_import_id: preview.skin_import_id,
                fingerprint: preview.fingerprint,
            },
        )
    }

    #[test]
    fn saved_skin_batch_preserves_exact_source_metadata_without_selecting_accounts() {
        let source = Fixture::new();
        let first = skin(11);
        let mut second = skin(22);
        second.0.applied_at = Some("2025-03-04T05:06:07.000Z".into());
        install(&source, &[first.clone(), second.clone()]);
        let before = snapshot(&source.baseline);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let accounts = AccountDirectory::new(Arc::clone(&destination.metadata)).unwrap();
        let account_before = accounts.create_offline_account("Current").unwrap();
        let settings = SettingsStore::new(Arc::clone(&destination.metadata)).unwrap();
        let settings_before = settings.current().unwrap();

        let imported = destination.commit(&prepared, &request).unwrap();
        assert!(!imported.already_imported);
        assert!(!imported.cutover_available);
        assert_eq!(imported.receipt.fingerprint, request.fingerprint);
        let mut expected_keys = vec![first.0.texture_key.clone(), second.0.texture_key.clone()];
        expected_keys.sort();
        assert_eq!(imported.receipt.texture_keys, expected_keys);
        for (record, bytes) in [&first, &second] {
            assert_eq!(
                destination
                    .skins
                    .get(&record.texture_key)
                    .unwrap()
                    .unwrap()
                    .record,
                *record
            );
            assert_eq!(
                destination
                    .skins
                    .read_png(&record.texture_key)
                    .unwrap()
                    .unwrap(),
                *bytes
            );
        }
        let active = accounts.capture_selected().unwrap();
        assert!(
            destination
                .skins
                .list_for_account(active.account_id())
                .unwrap()
                .iter()
                .all(|record| record.applied_at.is_none())
        );
        assert_eq!(accounts.snapshot().unwrap(), account_before);
        assert_eq!(settings.current().unwrap(), settings_before);
        assert_eq!(before, snapshot(&source.baseline));
    }

    #[test]
    fn saved_skin_receipt_survives_reopen_and_replay_preserves_later_edits_and_deletion() {
        let source = Fixture::new();
        let first = skin(11);
        let second = skin(22);
        install(&source, &[first.clone(), second.clone()]);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let first_commit = destination.commit(&prepared, &request).unwrap();
        assert!(matches!(
            destination
                .skins
                .delete_unapplied(&first.0.texture_key)
                .unwrap(),
            SavedSkinDeleteResult::Deleted(_)
        ));
        let edited = destination
            .skins
            .update_metadata(
                &second.0.texture_key,
                UpdateSavedSkinRequest {
                    name: Some("Edited after import".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        let Destination {
            skins,
            metadata,
            roots,
            root,
        } = destination;
        drop(skins);
        drop(metadata);
        let reopened = library(
            Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap()),
            &roots,
        );
        assert_eq!(
            skin_status(&reopened, &request.skin_import_id)
                .unwrap()
                .receipt,
            Some(first_commit.receipt.clone())
        );
        let replay = prepared
            .commit(&reopened, &request, &CancellationToken::new())
            .unwrap();
        assert!(replay.already_imported);
        assert_eq!(replay.receipt, first_commit.receipt);
        assert!(reopened.get(&first.0.texture_key).unwrap().is_none());
        assert_eq!(
            reopened.get(&second.0.texture_key).unwrap().unwrap().record,
            edited
        );
        drop(prepared);
        drop(source);
        assert_eq!(
            skin_status(&reopened, &request.skin_import_id)
                .unwrap()
                .receipt,
            Some(first_commit.receipt)
        );
    }

    #[test]
    fn saved_skin_drift_cancellation_and_wrong_request_leave_no_destination_effects() {
        let source = Fixture::new();
        let first = skin(11);
        install(&source, &[first.clone()]);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            prepared.commit(&destination.skins, &request, &cancel),
            Err(SkinImportError::Source(ImportError::Cancelled))
        ));
        let mut wrong = request.clone();
        wrong.skin_import_id = "f".repeat(64);
        assert!(matches!(
            destination.commit(&prepared, &wrong),
            Err(SkinImportError::Source(ImportError::SourceChanged))
        ));
        fs::write(
            source
                .baseline
                .join(format!("skins/files/{}.png", first.0.texture_key)),
            b"changed source",
        )
        .unwrap();
        let before = snapshot(&source.baseline);
        assert!(matches!(
            destination.commit(&prepared, &request),
            Err(SkinImportError::Source(ImportError::SourceChanged))
        ));
        assert!(destination.skins.list().unwrap().is_empty());
        assert!(
            skin_status(&destination.skins, &request.skin_import_id)
                .unwrap()
                .receipt
                .is_none()
        );
        assert_eq!(before, snapshot(&source.baseline));
    }

    #[test]
    fn saved_skin_readmitted_source_change_cannot_overwrite_completed_batch() {
        let source = Fixture::new();
        let mut first = skin(11);
        install(&source, &[first.clone()]);
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        let completed = destination.commit(&prepared, &request).unwrap();
        first.0.name = "Changed legacy metadata".into();
        install(&source, &[first]);
        let (changed, changed_request) = prepare(&source);
        assert_ne!(request.skin_import_id, changed_request.skin_import_id);
        assert!(matches!(
            destination.commit(&changed, &changed_request),
            Err(SkinImportError::Library(SkinLibraryError::Conflict))
        ));
        assert_eq!(destination.skins.list().unwrap()[0].name, "Saved skin 11");
        assert_eq!(
            skin_status(&destination.skins, &request.skin_import_id)
                .unwrap()
                .receipt,
            Some(completed.receipt)
        );
        assert!(
            skin_status(&destination.skins, &changed_request.skin_import_id)
                .unwrap()
                .receipt
                .is_none()
        );
    }

    #[test]
    fn saved_skin_validation_rejects_nonconcordant_or_unsupported_source_without_writes() {
        for case in [
            "schema",
            "unknown_index_field",
            "unknown_record_field",
            "duplicate",
            "size",
            "hash",
            "missing_png",
            "extra_png",
            "nested_directory",
            "legacy_png",
            "malformed_png",
        ] {
            let source = Fixture::new();
            let first = skin(11);
            install(&source, &[first.clone()]);
            let path = source
                .baseline
                .join(format!("skins/files/{}.png", first.0.texture_key));
            let mut index: Value = serde_json::from_slice(
                &fs::read(source.baseline.join("skins/index.json")).unwrap(),
            )
            .unwrap();
            match case {
                "schema" => index["schema_version"] = json!(4),
                "unknown_index_field" => index["pending"] = json!([]),
                "unknown_record_field" => index["skins"][0]["credentials"] = json!("not retained"),
                "duplicate" => {
                    let record = index["skins"][0].clone();
                    index["skins"].as_array_mut().unwrap().push(record);
                }
                "size" => index["skins"][0]["byte_size"] = json!(first.1.len() + 1),
                "hash" => {
                    fs::write(&path, &skin(22).1).unwrap();
                }
                "missing_png" => fs::remove_file(&path).unwrap(),
                "extra_png" => {
                    let other = skin(22);
                    fs::write(
                        source
                            .baseline
                            .join(format!("skins/files/{}.png", other.0.texture_key)),
                        other.1,
                    )
                    .unwrap();
                }
                "nested_directory" => {
                    fs::create_dir(source.baseline.join("skins/files/unknown")).unwrap()
                }
                "legacy_png" | "malformed_png" => {
                    let bytes = if case == "legacy_png" {
                        png(11, 32)
                    } else {
                        b"not a PNG".to_vec()
                    };
                    let key = texture_key(&bytes);
                    fs::remove_file(&path).unwrap();
                    fs::write(
                        source.baseline.join(format!("skins/files/{key}.png")),
                        &bytes,
                    )
                    .unwrap();
                    index["skins"][0]["texture_key"] = json!(key);
                    index["skins"][0]["byte_size"] = json!(bytes.len());
                }
                _ => unreachable!(),
            }
            write_index(&source, &index);
            let before = snapshot(&source.baseline);
            let previews = ImportPreviews::new();
            let preview = previews.admit(source.capture()).unwrap();
            assert!(!preview.skin_import_available, "{case}");
            assert!(
                previews.prepare_skins(&preview.fingerprint).is_err(),
                "{case}"
            );
            assert_eq!(before, snapshot(&source.baseline), "{case}");
        }
    }

    #[test]
    fn saved_skin_source_cannot_be_the_actual_destination_root_or_its_ancestor() {
        for nested in [false, true] {
            let source = Fixture::new();
            install(&source, &[skin(11)]);
            let destination = if nested {
                source.baseline.join("destination")
            } else {
                source.baseline.clone()
            };
            if nested {
                fs::create_dir(&destination).unwrap();
            }
            // Test-only root setup precedes capture. The production command must
            // do no work in this overlapping source after source admission.
            let roots = open_root(&destination);
            let skins = library(Arc::new(MetadataStore::in_memory().unwrap()), &roots);
            let (prepared, request) = prepare(&source);
            let before = snapshot(&source.baseline);
            assert!(matches!(
                prepared.commit(&skins, &request, &CancellationToken::new()),
                Err(SkinImportError::Source(ImportError::InvalidData))
            ));
            assert!(skins.list().unwrap().is_empty());
            assert!(
                skin_status(&skins, &request.skin_import_id)
                    .unwrap()
                    .receipt
                    .is_none()
            );
            assert_eq!(before, snapshot(&source.baseline));
        }
    }

    #[test]
    fn saved_skin_empty_index_and_sorted_capture_edges_are_exact() {
        let source = Fixture::new();
        assert!(!source.capture().preview().skin_import_available);
        install(&source, &[]);
        let inventory = source.capture();
        let manifests: Vec<_> = inventory.file_manifests().collect();
        for manifest in [manifests.first().unwrap(), manifests.last().unwrap()] {
            assert_eq!(
                texture_key(&inventory.record_bytes(&manifest.relative).unwrap()),
                manifest.sha256
            );
        }
        assert!(matches!(
            inventory.record_bytes("profile/absent.json"),
            Err(ImportError::InvalidData)
        ));
        let (prepared, request) = prepare(&source);
        let destination = Destination::new();
        for invalid in [
            "",
            "invalid",
            &"A".repeat(64),
            &format!(" {}", request.skin_import_id),
        ] {
            assert!(matches!(
                skin_status(&destination.skins, invalid),
                Err(SkinImportError::Source(ImportError::InvalidData))
            ));
        }
        let response = destination.commit(&prepared, &request).unwrap();
        assert!(response.receipt.texture_keys.is_empty());
        assert!(!response.already_imported);
        assert!(
            destination
                .commit(&prepared, &request)
                .unwrap()
                .already_imported
        );
    }
}
