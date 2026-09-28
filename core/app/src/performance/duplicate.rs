//! Read-only Performance admission for the instance owner's publication workflow.

use super::PerformanceMutationError;
use crate::{files::ScopedDirectory, instances::directory::RegisteredInstance};
use axial_performance::{ManagedDuplicateFile, ManagedDuplicatePayload};

pub(crate) struct PreparedDuplicate {
    source: RegisteredInstance,
    payload: ManagedDuplicatePayload,
}

impl PreparedDuplicate {
    pub(crate) fn admit(source: &RegisteredInstance) -> Result<Self, PerformanceMutationError> {
        source
            .validate_current()
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        let payload = ManagedDuplicatePayload::admit(source.directory().capability())
            .map_err(|_| PerformanceMutationError::Failed)?;
        Ok(Self {
            source: source.clone(),
            payload,
        })
    }

    pub(crate) fn files(&self) -> &[ManagedDuplicateFile] {
        self.payload.files()
    }

    pub(crate) fn witness(&self) -> String {
        self.payload.witness()
    }

    pub(crate) fn verify_staged(
        &self,
        destination: &ScopedDirectory,
    ) -> Result<(), PerformanceMutationError> {
        self.source
            .validate_current()
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        destination
            .revalidate()
            .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
        self.payload
            .verify_staged(destination.capability())
            .map_err(|_| PerformanceMutationError::Failed)
    }
}

pub(crate) fn is_reserved_mod_entry(name: &str) -> bool {
    ManagedDuplicatePayload::is_reserved_mod_entry(name)
}

pub(crate) fn verify_recovered(
    directory: &ScopedDirectory,
    witness: Option<&str>,
) -> Result<(), PerformanceMutationError> {
    directory
        .revalidate()
        .map_err(|_| PerformanceMutationError::InstanceUnavailable)?;
    ManagedDuplicatePayload::verify_recovered(directory.capability(), witness)
        .map_err(|_| PerformanceMutationError::Failed)
}

#[cfg(test)]
pub(crate) async fn seed_managed(
    source: &RegisteredInstance,
) -> axial_performance::CompositionState {
    use axial_performance::{
        CompositionPlan, CompositionTier, InstalledMod, ManagedArtifactIntegrity,
        ManagedArtifactPin, ManagedArtifactProvider, ManagedArtifactRole, ManagedArtifactSource,
        ManagedCompositionInstallPlan, ManagedRollbackOutcome, OwnershipClass, PerformanceManager,
        PerformanceMode,
        types::{ManagedMod, ModCondition},
    };
    use sha2::{Digest, Sha512};
    let manager = std::sync::Arc::new(PerformanceManager::new().unwrap());
    let (authority, identity) = manager
        .bind_admitted_instance(
            source.record().instance.id.as_str(),
            source.directory().capability().clone(),
        )
        .unwrap();
    let effects = authority
        .bind_instance_effect_authority(&identity)
        .await
        .unwrap();
    let plan = ManagedCompositionInstallPlan::seal(
        CompositionPlan {
            composition_id: "duplicate-fixture".into(),
            family: axial_performance::types::VersionFamily::F,
            loader: "fabric".into(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: Vec::new(),
            jvm_preset: String::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        },
        "1.21.4",
        "fabric",
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let mut state = authority
        .ensure_installed(
            &identity,
            &effects,
            &plan,
            super::public_transfer_resolver(),
            || async { Ok::<_, ()>(()) },
        )
        .await
        .unwrap()
        .into_state();
    // Seed a persisted, canonical graph offline; the leaf then creates its real
    // rollback history and restores the current artifact through its own effects.
    let bytes = b"independent managed artifact";
    let integrity = hex::encode(Sha512::digest(bytes));
    let declarative = CompositionPlan {
        composition_id: state.composition_id.clone(),
        family: state.family,
        loader: state.loader.clone(),
        mode: PerformanceMode::Managed,
        tier: state.tier,
        mods: vec![ManagedMod {
            artifact_id: "root".into(),
            project_id: "AANobbMI".into(),
            slug: String::new(),
            name: "Fixture".into(),
            condition: ModCondition::Always,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }],
        jvm_preset: String::new(),
        warnings: Vec::new(),
        fallback_reason: String::new(),
    };
    let managed_plan = ManagedCompositionInstallPlan::seal(
        declarative,
        &state.game_version,
        &state.loader,
        vec![
            ManagedArtifactPin::new(
                "AANobbMI",
                "NFkjnzWE",
                "current.jar",
                "https://example.invalid/current.jar",
                bytes.len() as u64,
                &integrity,
                ManagedArtifactRole::Root,
            )
            .unwrap(),
        ],
        Vec::new(),
    )
    .unwrap();
    state.graph_sha512 = managed_plan.graph_digest().to_owned();
    state.installed_mods = vec![InstalledMod {
        project_id: "AANobbMI".into(),
        version_id: "NFkjnzWE".into(),
        filename: "current.jar".into(),
        role: ManagedArtifactRole::Root,
        size: bytes.len() as u64,
        ownership_class: OwnershipClass::CompositionManaged,
        source: ManagedArtifactSource {
            provider: ManagedArtifactProvider::Modrinth,
        },
        integrity: ManagedArtifactIntegrity { sha512: integrity },
    }];
    let mods = source.directory().read_projection().unwrap().join("mods");
    let lock_path = mods.join(".axial-lock.json");
    let mut encoded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    encoded["state"] = serde_json::to_value(&state).unwrap();
    std::fs::write(mods.join("current.jar"), bytes).unwrap();
    std::fs::write(lock_path, serde_json::to_vec_pretty(&encoded).unwrap()).unwrap();
    authority.remove_managed(&identity, &effects).await.unwrap();
    match authority
        .rollback_managed(&identity, &effects)
        .await
        .unwrap()
    {
        ManagedRollbackOutcome::ManagedComposition(state) => state,
        ManagedRollbackOutcome::ManagedStateAbsent => {
            panic!("fixture managed state was not restored")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::{
        create::tests::{create, fixture, payload_path},
        duplicate::DuplicateRequest,
    };
    use axial_performance::{ManagedRollbackOutcome, PerformanceManager, RollbackSnapshotTarget};

    #[tokio::test]
    async fn duplicate_preserves_managed_history_and_rollback_changes_only_the_new_instance() {
        let (_root, service) = fixture();
        let source = create(&service, "Managed source").await;
        let source_path = payload_path(&service, &source.id);
        std::fs::write(source_path.join("mods/user.jar"), b"unmanaged mod bytes").unwrap();
        let source_admission = service.directories().admit(&source.id).unwrap();
        let state = seed_managed(&source_admission).await;
        let source_files = snapshot_files(&source_path);
        drop(source_admission);
        let copied = service
            .duplicate(&source.id, DuplicateRequest::default())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let copied_path = payload_path(&service, &copied.id);
        let copy = service.directories().admit(&copied.id).unwrap();
        let manager = std::sync::Arc::new(PerformanceManager::new().unwrap());
        let (authority, identity) = manager
            .bind_admitted_instance(copied.id.as_str(), copy.directory().capability().clone())
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
        let managed = inspection
            .rollback_snapshots
            .iter()
            .find(|snapshot| snapshot.target == RollbackSnapshotTarget::ManagedComposition)
            .unwrap();
        assert_eq!(
            authority
                .rollback_managed_snapshot(&identity, &effects, &absent.id)
                .await
                .unwrap(),
            ManagedRollbackOutcome::ManagedStateAbsent
        );
        assert!(!copied_path.join("mods/.axial-lock.json").exists());
        assert!(matches!(
            authority
                .rollback_managed_snapshot(&identity, &effects, &managed.id)
                .await
                .unwrap(),
            ManagedRollbackOutcome::ManagedComposition(_)
        ));
        assert!(copied_path.join("mods/.axial-lock.json").is_file());
        assert_eq!(
            std::fs::read(copied_path.join("mods/user.jar")).unwrap(),
            b"unmanaged mod bytes"
        );
        assert_eq!(snapshot_files(&source_path), source_files);
    }

    fn snapshot_files(
        root: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    directories.push(entry.path());
                } else {
                    files.insert(
                        entry.path().strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
        files
    }
}
