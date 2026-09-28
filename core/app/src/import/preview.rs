use super::model::InstanceImportMappings;
use super::{
    ImportError, ImportPreview, ImportResult, Inventory, PreparedInstanceImport,
    PreparedMetadataImport, PreparedSkinImport,
};
use crate::instances::{create::InstanceService, model::InstanceError};
use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
};

/// One in-memory preview admitted by composition/native selection. HTTP callers
/// cannot turn arbitrary path strings into source capabilities. This object
/// retains no metadata, keyring or payload-write owner.
#[derive(Default)]
pub struct ImportPreviews {
    current: RwLock<Option<Arc<Inventory>>>,
}

impl ImportPreviews {
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a captured display snapshot without filesystem I/O. Capture
    /// validated it; current/prepare still revalidate before subsequent use.
    /// The in-memory swap is not authority to publish an instance.
    pub fn admit(&self, inventory: Inventory) -> ImportResult<ImportPreview> {
        let inventory = inventory.admit_snapshot()?;
        let preview = inventory.preview();
        *self.current.write().map_err(|_| ImportError::Unavailable)? = Some(Arc::new(inventory));
        Ok(preview)
    }

    pub fn current(&self) -> ImportResult<ImportPreview> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        inventory.revalidate()?;
        Ok(inventory.preview())
    }

    pub fn forget(&self) -> ImportResult<()> {
        *self.current.write().map_err(|_| ImportError::Unavailable)? = None;
        Ok(())
    }

    /// A request can select only an instance within the currently admitted,
    /// unchanged preview. It cannot introduce a path or destination authority.
    pub fn prepare_instance(
        &self,
        fingerprint: &str,
        legacy_id: &str,
    ) -> ImportResult<PreparedInstanceImport> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        inventory.prepare_instance(fingerprint, legacy_id)
    }

    pub fn instance_mappings(
        &self,
        fingerprint: &str,
        instances: &InstanceService,
    ) -> ImportResult<InstanceImportMappings> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        inventory.revalidate()?;
        if inventory.fingerprint() != fingerprint {
            return Err(ImportError::SourceChanged);
        }
        let mappings = instances
            .completed_import_mappings(&inventory.source_identity()?, fingerprint)
            .map_err(|error| match error {
                InstanceError::InvalidId
                | InstanceError::InvalidInput
                | InstanceError::InvalidName
                | InstanceError::InvalidSettings => ImportError::InvalidData,
                _ => ImportError::Unavailable,
            })?;
        let source_ids: BTreeSet<_> = inventory
            .instances()
            .iter()
            .map(|instance| instance.legacy_id.as_str())
            .collect();
        if mappings.keys().any(|id| !source_ids.contains(id.as_str())) {
            return Err(ImportError::InvalidData);
        }
        let metadata_import_id = inventory.metadata_import_id()?;
        inventory.revalidate()?;
        let current = self.current.read().map_err(|_| ImportError::Unavailable)?;
        match current.as_ref() {
            None => return Err(ImportError::NoSource),
            Some(current) if !Arc::ptr_eq(current, &inventory) => {
                return Err(ImportError::SourceChanged);
            }
            _ => (),
        }
        Ok(InstanceImportMappings {
            fingerprint: fingerprint.to_owned(),
            metadata_import_id,
            instance_id_mapping: mappings,
            cutover_available: false,
        })
    }

    pub fn prepare_metadata(&self, fingerprint: &str) -> ImportResult<PreparedMetadataImport> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        inventory.prepare_metadata(fingerprint)
    }

    pub fn prepare_skins(&self, fingerprint: &str) -> ImportResult<PreparedSkinImport> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        inventory.prepare_skins(fingerprint)
    }

    pub fn prepare_rules(
        &self,
        fingerprint: &str,
    ) -> ImportResult<super::rules::PreparedRulesImport> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        let prepared = inventory.prepare_rules(fingerprint)?;
        self.ensure_current(&inventory)?;
        Ok(prepared)
    }

    pub fn current_with_rules(
        &self,
        rules: &crate::performance::rules::PerformanceRules,
    ) -> ImportResult<ImportPreview> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        let preview = inventory.rules_preview(rules)?;
        self.ensure_current(&inventory)?;
        Ok(preview)
    }

    pub fn prepare_instance_with_rules(
        &self,
        fingerprint: &str,
        legacy_id: &str,
        rules: &crate::performance::rules::PerformanceRules,
    ) -> ImportResult<PreparedInstanceImport> {
        let inventory = self
            .current
            .read()
            .map_err(|_| ImportError::Unavailable)?
            .clone()
            .ok_or(ImportError::NoSource)?;
        inventory.revalidate()?;
        if inventory.fingerprint() != fingerprint {
            return Err(ImportError::SourceChanged);
        }
        let completed = rules
            .completed_import(&inventory.source_identity()?, fingerprint)
            .map_err(|_| ImportError::Unavailable)?;
        let prepared =
            inventory.prepare_instance_with_rules(fingerprint, legacy_id, completed.as_ref())?;
        self.ensure_current(&inventory)?;
        Ok(prepared)
    }

    fn ensure_current(&self, inventory: &Arc<Inventory>) -> ImportResult<()> {
        let current = self.current.read().map_err(|_| ImportError::Unavailable)?;
        match current.as_ref() {
            None => Err(ImportError::NoSource),
            Some(current) if !Arc::ptr_eq(current, inventory) => Err(ImportError::SourceChanged),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        import::tests::{Fixture, snapshot},
        storage::{
            StorageError,
            rusqlite::hooks::{AuthAction, AuthContext, Authorization},
        },
    };

    #[test]
    fn rules_preview_admission_failure_preserves_the_previous_selection() {
        use crate::{
            performance::rules::{MIGRATION, PerformanceRules},
            storage::MetadataStore,
        };
        let old = Fixture::new();
        let next = Fixture::new();
        let previews = ImportPreviews::new();
        let old_preview = previews.admit(old.capture()).unwrap();
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage.migrate(&[MIGRATION]).unwrap(); // Deliberately missing completed-fact metadata.
        let rules = PerformanceRules::with_remote(storage, None, None).unwrap();
        let before = snapshot(&next.baseline);
        let result = next
            .capture()
            .resolve_rules_preview(&rules)
            .and_then(|inventory| previews.admit(inventory));
        assert!(matches!(result, Err(ImportError::Unavailable)));
        assert_eq!(previews.current().unwrap(), old_preview);
        assert_eq!(snapshot(&next.baseline), before);
    }

    #[tokio::test]
    async fn rules_preview_admission_uses_completed_proof_and_withdraws_missing_fact() {
        use crate::{
            performance::rules::{
                IMPORT_MIGRATION, MIGRATION, PerformanceRules, tests::signed_cache,
            },
            storage::MetadataStore,
            tasks::CancellationToken,
        };
        let source = Fixture::new();
        let (bytes, key) = signed_cache("2001-01-01T00:00:00Z");
        std::fs::create_dir_all(source.baseline.join("performance")).unwrap();
        std::fs::write(source.baseline.join("performance/rules-cache.json"), bytes).unwrap();
        let before = snapshot(&source.baseline);
        let (root, service) = crate::instances::create::tests::fixture();
        let storage = Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
        storage.migrate(&[MIGRATION, IMPORT_MIGRATION]).unwrap();
        let rules = PerformanceRules::with_remote(
            storage.clone(),
            Some("https://example.invalid/rules".into()),
            Some(key),
        )
        .unwrap();
        assert!(rules.uses_metadata(&storage));
        assert!(!rules.uses_metadata(&Arc::new(MetadataStore::in_memory().unwrap())));
        let previews = ImportPreviews::new();
        let preview = previews
            .admit(source.capture().resolve_rules_preview(&rules).unwrap())
            .unwrap();
        assert!(!preview.instances[0].ordinary_import_available);
        let request = super::super::model::RulesImportRequest {
            fingerprint: preview.fingerprint.clone(),
            rules_import_id: preview.rules_import_id.clone(),
        };
        previews
            .prepare_rules(&preview.fingerprint)
            .unwrap()
            .commit(
                &rules,
                &service
                    .directories()
                    .library()
                    .admit_application_root()
                    .unwrap(),
                &request,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let resolved = source.capture().resolve_rules_preview(&rules).unwrap();
        assert_eq!(resolved.preview().fingerprint, preview.fingerprint);
        assert!(resolved.preview().instances[0].ordinary_import_available);
        let admitted = previews.admit(resolved).unwrap();
        assert_eq!(admitted, previews.current_with_rules(&rules).unwrap());
        let prepared = previews
            .prepare_instance_with_rules(&preview.fingerprint, "0000000000000001", &rules)
            .unwrap();
        assert!(
            service
                .import_instance(prepared)
                .unwrap()
                .join()
                .await
                .unwrap()
                .is_ok()
        );
        storage
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute("DELETE FROM performance_rules_imports", [])?;
                Ok(())
            })
            .unwrap();
        assert!(
            !previews.current_with_rules(&rules).unwrap().instances[0].ordinary_import_available
        );
        assert!(
            previews
                .prepare_instance_with_rules(&preview.fingerprint, "0000000000000001", &rules)
                .is_err()
        );
        assert_eq!(snapshot(&source.baseline), before);
    }

    #[tokio::test]
    async fn instance_mappings_require_exact_admitted_source_even_for_identical_content() {
        let source = Fixture::new();
        let other = Fixture::new();
        let before = snapshot(&source.baseline);
        let previews = ImportPreviews::new();
        let (_root, instances) = crate::instances::create::tests::fixture();
        assert!(matches!(
            previews.instance_mappings("", &instances),
            Err(ImportError::NoSource)
        ));
        let preview = previews.admit(source.capture()).unwrap();
        for fingerprint in ["", "invalid", &"0".repeat(64)] {
            assert!(matches!(
                previews.instance_mappings(fingerprint, &instances),
                Err(ImportError::SourceChanged)
            ));
        }
        assert!(
            previews
                .instance_mappings(&preview.fingerprint, &instances)
                .unwrap()
                .instance_id_mapping
                .is_empty()
        );
        let instance = instances
            .import_instance(
                previews
                    .prepare_instance(&preview.fingerprint, "0000000000000001")
                    .unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let mapped = previews
            .instance_mappings(&preview.fingerprint, &instances)
            .unwrap();
        assert_eq!(mapped.metadata_import_id, preview.metadata_import_id);
        assert_eq!(
            mapped.instance_id_mapping.get("0000000000000001"),
            Some(&instance.id.to_string())
        );
        assert!(!mapped.cutover_available);
        assert_eq!(snapshot(&source.baseline), before);
        let other_preview = previews.admit(other.capture()).unwrap();
        assert_eq!(other_preview.fingerprint, preview.fingerprint);
        assert_ne!(other_preview.metadata_import_id, preview.metadata_import_id);
        let other_mapping = previews
            .instance_mappings(&other_preview.fingerprint, &instances)
            .unwrap();
        assert_eq!(
            other_mapping.metadata_import_id,
            other_preview.metadata_import_id
        );
        assert!(other_mapping.instance_id_mapping.is_empty());
        previews.forget().unwrap();
        assert!(matches!(
            previews.instance_mappings(&other_preview.fingerprint, &instances),
            Err(ImportError::NoSource)
        ));
        previews.admit(source.capture()).unwrap();
        std::fs::write(source.baseline.join("config.json"), b"{}").unwrap();
        assert!(matches!(
            previews.instance_mappings(&preview.fingerprint, &instances),
            Err(ImportError::SourceChanged)
        ));
    }

    #[test]
    fn instance_mappings_recheck_source_and_current_preview_after_metadata_query() {
        for change in ["forget", "replace", "source"] {
            let source = Fixture::new();
            let previews = Arc::new(ImportPreviews::new());
            let preview = previews.admit(source.capture()).unwrap();
            let next = source.capture();
            let (_root, instances) = crate::instances::create::tests::fixture();
            let during_query = previews.clone();
            let source_file = source.baseline.join("config.json");
            let mut replacement = Some(next);
            let mut changed = false;
            instances
                .registry()
                .storage()
                .read(|connection| -> Result<_, StorageError> {
                    connection.authorizer(Some(move |context: AuthContext<'_>| {
                        if !changed
                            && matches!(
                                context.action,
                                AuthAction::Read {
                                    table_name: "instance_imports",
                                    ..
                                }
                            )
                        {
                            changed = true;
                            match change {
                                "forget" => during_query.forget().unwrap(),
                                "replace" => {
                                    during_query.admit(replacement.take().unwrap()).unwrap();
                                }
                                _ => std::fs::write(&source_file, b"{}").unwrap(),
                            }
                        }
                        Authorization::Allow
                    }));
                    Ok(())
                })
                .unwrap();
            let result = previews.instance_mappings(&preview.fingerprint, &instances);
            if change == "forget" {
                assert!(matches!(result, Err(ImportError::NoSource)));
            } else {
                assert!(
                    matches!(result, Err(ImportError::SourceChanged)),
                    "{change}"
                );
            }
            instances
                .registry()
                .storage()
                .read(|connection| -> Result<_, StorageError> {
                    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
                    Ok(())
                })
                .unwrap();
        }
    }
}
