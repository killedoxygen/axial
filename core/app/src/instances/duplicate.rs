//! A duplicate becomes visible only after its independent payload is published.
//!
//! Display names are metadata, never directory authority. The source remains
//! unchanged, and missing optional source files stay missing in the copy.

use std::collections::HashSet;

use super::{
    copy::{CopyBudget, check_cancel, copy_file, copy_tree, copy_tree_excluding},
    create::{DuplicateSource, InstanceService},
    directory::{InstanceDirectories, RegisteredInstance},
    model::{Instance, InstanceError, InstanceId, InstanceResult},
};
use crate::{
    files::{PortableName, ScopedDirectory},
    performance::duplicate::{PreparedDuplicate, is_reserved_mod_entry},
    tasks::{CancellationToken, TaskHandle},
};
use serde::{Deserialize, Serialize};

impl InstanceService {
    pub fn duplicate(
        &self,
        id: &InstanceId,
        request: DuplicateRequest,
    ) -> InstanceResult<TaskHandle<InstanceResult<Instance>>> {
        let pin = self
            .directories
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let new_id = InstanceId::new();
        let source_lease = self
            .directories
            .exclusions()
            .try_acquire([id.as_str()], [])
            .map_err(|_| InstanceError::Busy)?;
        let lease = self
            .directories
            .exclusions()
            .try_acquire([new_id.as_str()], [])
            .map_err(|_| InstanceError::Busy)?;
        if crate::content::install::has_pending(self.registry().storage(), id)? {
            return Err(InstanceError::Busy);
        }
        if crate::performance::mutation::has_pending(self.registry().storage(), id)? {
            return Err(InstanceError::Busy);
        }
        if super::setup::has_pending(self.registry().storage(), id)? {
            return Err(InstanceError::Busy);
        }
        let source = InstanceDirectories::admit_record(
            self.registry().clone(),
            self.registry().get_live(id)?,
            pin.clone(),
            source_lease,
        )?;
        let names = self
            .registry()
            .list()?
            .into_iter()
            .chain(self.registry().pending()?)
            .map(|record| record.instance.name)
            .collect::<Vec<_>>();
        let mut instance = source.record().instance.clone();
        instance.id = new_id;
        instance.name = choose_duplicate_name(&instance.name, &names, request.name.as_deref())
            .map_err(|_| InstanceError::NameConflict)?;
        let service = self.for_operation(instance.id.clone());
        self.tasks
            .try_spawn(
                (pin.clone(), lease.clone(), source.clone()),
                move |cancel| async move {
                    tokio::task::spawn_blocking(move || {
                        check_cancel(&cancel)?;
                        let performance = PreparedDuplicate::admit(&source)
                            .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
                        service.publish_instance(
                            instance,
                            pin,
                            lease,
                            Some(DuplicateSource {
                                source,
                                performance,
                            }),
                            None,
                            cancel,
                        )
                    })
                    .await
                    .unwrap_or_else(|error| {
                        if error.is_panic() {
                            std::panic::resume_unwind(error.into_panic());
                        }
                        Err(InstanceError::Cancelled)
                    })
                },
            )
            .map_err(|_| InstanceError::Closed)
    }
}

/// The retained duplicate contract copies user content, with independent bytes.
/// Symbolic links, device entries, portable aliases and oversized trees fail.
pub(crate) fn copy_payload(
    service: &InstanceService,
    source: &RegisteredInstance,
    performance: &PreparedDuplicate,
    destination: &ScopedDirectory,
    cancel: &CancellationToken,
) -> InstanceResult<()> {
    let mut budget = CopyBudget {
        entries: 100_000,
        bytes: 128 * 1024 * 1024 * 1024,
        file_bytes: 4 * 1024 * 1024 * 1024,
    };
    for name in COPIED_DIRECTORIES {
        check_cancel(cancel)?;
        let name = PortableName::new_exact(name).expect("fixed portable name");
        let target = service.fresh_directory(destination, &name)?;
        match source.directory().open_directory(&name) {
            Ok(directory) if name.as_str() == "mods" => copy_tree_excluding(
                service,
                directory.capability(),
                &target,
                cancel,
                &mut budget,
                0,
                is_reserved_mod_entry,
            )?,
            Ok(directory) => copy_tree(
                service,
                directory.capability(),
                &target,
                cancel,
                &mut budget,
                0,
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(InstanceError::DirectoryUnavailable),
        }
    }
    for name in COPIED_FILES {
        let name = PortableName::new_exact(name).expect("fixed portable name");
        copy_file(
            service,
            source.directory().capability(),
            destination,
            &name,
            cancel,
            &mut budget,
            true,
        )?;
    }
    for name in EMPTY_DIRECTORIES {
        service.fresh_directory(
            destination,
            &PortableName::new_exact(name).expect("fixed portable name"),
        )?;
    }
    for file in performance.files() {
        check_cancel(cancel)?;
        budget.entries = budget
            .entries
            .checked_sub(1)
            .ok_or(InstanceError::InvalidInput)?;
        let (_, parents) = file
            .relative_components()
            .split_last()
            .ok_or(InstanceError::ManagedDuplicateUnavailable)?;
        let mut parent = destination.clone();
        for component in parents {
            let name = PortableName::new_exact(component)
                .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
            parent = match parent.open_directory(&name) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    service.fresh_directory(&parent, &name)?
                }
                Err(_) => return Err(InstanceError::DirectoryUnavailable),
            };
        }
        let name = PortableName::new_exact(file.filename())
            .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
        copy_file(
            service,
            file.source_directory(),
            &parent,
            &name,
            cancel,
            &mut budget,
            false,
        )?;
    }
    Ok(())
}

/// Retained API input: omitting the body or a blank name chooses a suffix.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuplicateRequest {
    pub name: Option<String>,
}

pub(crate) const COPIED_DIRECTORIES: [&str; 5] =
    ["mods", "saves", "resourcepacks", "shaderpacks", "config"];
pub(crate) const COPIED_FILES: [&str; 2] = ["options.txt", "servers.dat"];
pub(crate) const EMPTY_DIRECTORIES: [&str; 2] = ["screenshots", "logs"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DuplicateNameError {
    Conflict,
}

/// The registry calls this inside the reservation transaction, including every
/// pending name in `occupied`. A read-before-reserve check is insufficient.
pub(crate) fn choose_duplicate_name(
    source_name: &str,
    occupied: &[String],
    requested: Option<&str>,
) -> Result<String, DuplicateNameError> {
    let occupied: HashSet<&str> = occupied.iter().map(String::as_str).collect();
    if let Some(name) = requested.map(str::trim).filter(|name| !name.is_empty()) {
        return if occupied.contains(name) {
            Err(DuplicateNameError::Conflict)
        } else {
            Ok(name.to_owned())
        };
    }
    let base = format!("{source_name} copy");
    if !occupied.contains(base.as_str()) {
        return Ok(base);
    }
    // At most `occupied.len()` suffixes can be occupied. The extra candidate
    // guarantees a free name without an unbounded search.
    for index in 2..=occupied.len().saturating_add(2) {
        let name = format!("{base} {index}");
        if !occupied.contains(name.as_str()) {
            return Ok(name);
        }
    }
    Err(DuplicateNameError::Conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        instances::{
            create::tests::{create, fixture, payload_path},
            directory::Registry,
        },
        library::{LibraryId, LibraryLifecycle, LibraryOpenOutcome},
        performance::duplicate::seed_managed,
        storage::{MetadataStore, rusqlite::params},
        tasks::{Exclusions, TaskOwner},
    };
    use std::{collections::BTreeMap, path::Path, sync::Arc};

    fn tree_bytes(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn collect(
            root: &Path,
            directory: &Path,
            files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    collect(root, &entry.path(), files);
                } else {
                    files.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        collect(root, root, &mut files);
        files
    }

    fn reopen(root: &Path, library_id: LibraryId) -> InstanceService {
        let library = match LibraryLifecycle::open_with_id(root, library_id) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("restarted duplicate fixture: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
        InstanceService::new(
            InstanceDirectories::new(Registry::new(storage), library, Exclusions::new()),
            TaskOwner::new(16).unwrap(),
        )
    }

    fn interrupted(service: &InstanceService, source: &InstanceId, phase: &str) -> InstanceId {
        let source = service.directories.admit(source).unwrap();
        let performance = PreparedDuplicate::admit(&source).unwrap();
        let pin = service.directories.library().admit().unwrap();
        let id = InstanceId::new();
        let stage_name = format!("stage-{id}");
        let record = service.registry().storage().transaction(|tx| -> InstanceResult<_> {
            let record = service.registry().reserve_duplicate(
                tx,
                source.record(),
                id.clone(),
                None,
                &pin.library_id().to_string(),
            )?;
            tx.execute(
                "INSERT INTO instance_creations(instance_id,record_json,stage_name,phase) VALUES(?1,?2,?3,'building')",
                params![id.as_str(), serde_json::to_string(&record).unwrap(), stage_name],
            )?;
            Ok(record)
        }).unwrap();
        let parent = service.ensure_parent(&pin).unwrap();
        let stage = service
            .fresh_directory(&parent, &PortableName::new_exact(&stage_name).unwrap())
            .unwrap();
        copy_payload(
            service,
            &source,
            &performance,
            &stage,
            &CancellationToken::new(),
        )
        .unwrap();
        performance.verify_staged(&stage).unwrap();
        let witness = (phase != "building").then(|| performance.witness());
        service
            .registry()
            .storage()
            .transaction(|tx| -> InstanceResult<()> {
                tx.execute(
                "UPDATE instance_creations SET phase=?2,directory_receipt=?3,performance_witness=?4 WHERE instance_id=?1",
                params![id.as_str(), phase, stage.receipt().unwrap(), witness],
            )?;
                Ok(())
            })
            .unwrap();
        if phase == "published" {
            let (outcome, _) = stage
                .move_no_replace(
                    &parent,
                    &PortableName::new_exact(&record.directory_name).unwrap(),
                )
                .into_parts();
            assert!(matches!(
                outcome,
                axial_fs::DirectoryMoveOutcome::Applied(_)
            ));
        }
        id
    }

    #[tokio::test]
    async fn accepted_managed_duplicate_cancellation_preserves_source_and_releases_admission() {
        let (_root, service) = fixture();
        let source = create(&service, "Managed source").await;
        let admitted = service.directories.admit(&source.id).unwrap();
        seed_managed(&admitted).await;
        drop(admitted);
        let source_path = payload_path(&service, &source.id);
        let before = tree_bytes(&source_path);
        let task = service
            .duplicate(&source.id, DuplicateRequest::default())
            .unwrap();
        assert!(task.cancel());
        assert!(matches!(
            task.join().await.unwrap(),
            Err(InstanceError::Cancelled)
        ));
        assert_eq!(service.registry().list().unwrap().len(), 1);
        assert!(service.registry().pending().unwrap().is_empty());
        assert!(service.pending().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
        assert_eq!(tree_bytes(&source_path), before);
        service
            .directories
            .admit(&source.id)
            .unwrap()
            .validate_current()
            .unwrap();
    }

    #[tokio::test]
    async fn managed_duplicate_restart_cancels_building_and_finishes_verified_publication() {
        for phase in ["building", "ready", "published"] {
            let (root, service) = fixture();
            let source = create(&service, "Managed source").await;
            let admitted = service.directories.admit(&source.id).unwrap();
            seed_managed(&admitted).await;
            drop(admitted);
            let source_path = payload_path(&service, &source.id);
            let before = tree_bytes(&source_path);
            let id = interrupted(&service, &source.id, phase);
            let library_id = service.directories.library().admit().unwrap().library_id();
            assert_eq!(service.registry().list().unwrap().len(), 1);
            drop(service);

            let service = reopen(root.path(), library_id);
            let recovered = service
                .recover_creation(&id)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            if phase == "building" {
                assert!(recovered.is_none());
                assert!(
                    !source_path
                        .parent()
                        .unwrap()
                        .join(format!("stage-{id}"))
                        .exists()
                );
                assert!(!payload_path(&service, &id).exists());
                assert_eq!(service.registry().list().unwrap().len(), 1);
            } else {
                assert_eq!(recovered.unwrap().id, id);
                let source = service.directories.admit(&source.id).unwrap();
                let copied = service.directories.admit(&id).unwrap();
                PreparedDuplicate::admit(&source)
                    .unwrap()
                    .verify_staged(copied.directory())
                    .unwrap();
                let manager = Arc::new(axial_performance::PerformanceManager::new().unwrap());
                let (authority, identity) = manager
                    .bind_admitted_instance(id.as_str(), copied.directory().capability().clone())
                    .unwrap();
                let effects = authority
                    .bind_instance_effect_authority(&identity)
                    .await
                    .unwrap();
                authority.remove_managed(&identity, &effects).await.unwrap();
                assert!(matches!(
                    authority
                        .rollback_managed(&identity, &effects)
                        .await
                        .unwrap(),
                    axial_performance::ManagedRollbackOutcome::ManagedComposition(_)
                ));
                assert_eq!(service.registry().list().unwrap().len(), 2);
            }
            assert_eq!(tree_bytes(&source_path), before);
            assert!(service.pending().unwrap().is_empty());
            assert!(!service.has_unsettled_effects());
        }
    }

    #[tokio::test]
    async fn changed_managed_payload_after_ready_stays_preserved_and_unavailable_on_restart() {
        for phase in ["ready", "published"] {
            for change in [
                "lock changed",
                "lock missing",
                "history changed",
                "history missing",
                "history artifact changed",
                "history artifact missing",
                "artifact changed",
                "artifact missing",
                "witness missing",
                "witness malformed",
            ] {
                let (root, service) = fixture();
                let source = create(&service, "Managed source").await;
                let admitted = service.directories.admit(&source.id).unwrap();
                let state = seed_managed(&admitted).await;
                drop(admitted);
                let source_path = payload_path(&service, &source.id);
                let source_before = tree_bytes(&source_path);
                let id = interrupted(&service, &source.id, phase);
                let destination = if phase == "ready" {
                    source_path.parent().unwrap().join(format!("stage-{id}"))
                } else {
                    payload_path(&service, &id)
                };
                if change.starts_with("witness") {
                    service.registry().storage().transaction(|tx| -> InstanceResult<()> {
                        tx.execute(
                            "UPDATE instance_creations SET performance_witness=?2 WHERE instance_id=?1",
                            params![id.as_str(), (change == "witness malformed").then_some("not-a-proof")],
                        )?;
                        Ok(())
                    }).unwrap();
                } else {
                    let file = if change.starts_with("lock") {
                        destination.join("mods/.axial-lock.json")
                    } else if change.starts_with("history") {
                        destination.join(
                            tree_bytes(&destination)
                                .keys()
                                .find(|path| {
                                    path.starts_with("mods/.axial-performance/rollback/history")
                                        && path.file_name().is_some_and(|name| {
                                            if change.starts_with("history artifact") {
                                                name != "snapshot.json"
                                            } else {
                                                name == "snapshot.json"
                                            }
                                        })
                                })
                                .unwrap(),
                        )
                    } else {
                        destination
                            .join("mods")
                            .join(&state.installed_mods[0].filename)
                    };
                    if change.ends_with("missing") {
                        std::fs::remove_file(&file).unwrap();
                    } else if change.contains("artifact") {
                        std::fs::write(&file, b"foreign managed-file bytes").unwrap();
                    } else {
                        let mut metadata: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                        if change.starts_with("lock") {
                            metadata["state"]["installed_at"] = "2030-01-01T00:00:00Z".into();
                        } else {
                            metadata["created_at"] = "2030-01-01T00:00:00Z".into();
                        }
                        std::fs::write(&file, serde_json::to_vec(&metadata).unwrap()).unwrap();
                    }
                }
                if matches!(change, "lock changed" | "history changed") {
                    let pin = service.directories.library().admit().unwrap();
                    let directory = service
                        .ensure_parent(&pin)
                        .unwrap()
                        .open_directory(
                            &PortableName::new_exact(
                                destination.file_name().unwrap().to_str().unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    assert!(
                        axial_performance::ManagedDuplicatePayload::admit(directory.capability())
                            .is_ok()
                    );
                }
                let destination_before = tree_bytes(&destination);
                let library_id = service.directories.library().admit().unwrap().library_id();
                drop(service);

                let service = reopen(root.path(), library_id);
                assert!(
                    matches!(
                        service.recover_creation(&id).unwrap().join().await.unwrap(),
                        Err(InstanceError::ManagedDuplicateUnavailable)
                    ),
                    "{phase}: {change}"
                );
                assert_eq!(service.registry().list().unwrap().len(), 1);
                assert!(service.registry().get_live(&id).is_err());
                assert_eq!(service.pending().unwrap()[0].instance_id, id);
                assert_eq!(
                    tree_bytes(&destination),
                    destination_before,
                    "{phase}: {change}"
                );
                assert_eq!(tree_bytes(&source_path), source_before, "{phase}: {change}");
                if phase == "ready" {
                    assert!(!payload_path(&service, &id).exists());
                }
            }
        }
    }

    #[test]
    fn default_names_follow_retained_copy_suffixes_and_fill_holes() {
        let occupied = vec!["Pack".into(), "Pack copy".into(), "Pack copy 3".into()];
        assert_eq!(
            choose_duplicate_name("Pack", &occupied, None),
            Ok("Pack copy 2".into())
        );
        assert_eq!(
            choose_duplicate_name("Pack", &occupied, Some(" \t ")),
            Ok("Pack copy 2".into())
        );
        assert_eq!(
            choose_duplicate_name("Other", &occupied, None),
            Ok("Other copy".into())
        );
    }

    #[test]
    fn explicit_names_trim_and_reject_conflicts_without_silently_renaming() {
        let occupied = vec!["Pack".into(), "Pending copy".into()];
        assert_eq!(
            choose_duplicate_name("Pack", &occupied, Some(" Pending copy ")),
            Err(DuplicateNameError::Conflict)
        );
        assert_eq!(
            choose_duplicate_name("Pack", &occupied, Some(" My copy \n")),
            Ok("My copy".into())
        );
        assert_eq!(
            choose_duplicate_name("Pack", &occupied, Some("pack")),
            Ok("pack".into())
        );
    }

    #[test]
    fn missing_name_uses_defaults_and_input_cannot_supply_destination_authority() {
        assert_eq!(
            serde_json::from_str::<DuplicateRequest>("{}").unwrap(),
            DuplicateRequest::default()
        );
        assert!(
            serde_json::from_str::<DuplicateRequest>(r#"{"name":"Copy","path":"/elsewhere"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<DuplicateRequest>(r#"{"name":42}"#).is_err());
    }
}
