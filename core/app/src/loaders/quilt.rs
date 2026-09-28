//! Quilt's application boundary over the retained Minecraft implementation.
//!
//! The provider owns exact loader, hashed and intermediary mapping proofs. The
//! retained installer validates those proofs, preserves the authenticated base's
//! Java requirement, resolves library conflicts and seals freshly downloaded
//! checksumless JARs before publication. Those algorithms have one owner.

use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{
    DownloadProgress, LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderError,
    LoaderGameVersion, LoaderInstallError, LoaderInstallPublicationOutcome, ManagedRuntimeCache,
};

const COMPONENT: LoaderComponentId = LoaderComponentId::Quilt;

/// Includes provider game stability, Minecraft enrichment and cache freshness.
pub async fn fetch_supported_versions(
    operation: &ManagedLibraryOperation,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    axial_minecraft::fetch_supported_versions(operation, COMPONENT).await
}

/// Retains upstream build metadata and normalized default selection ordering.
pub async fn fetch_builds(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    axial_minecraft::fetch_builds(operation, COMPONENT, minecraft_version).await
}

pub fn fetch_cached_builds(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<Option<(Vec<LoaderBuildRecord>, LoaderCatalogState)>, LoaderError> {
    axial_minecraft::fetch_cached_builds(operation, COMPONENT, minecraft_version)
}

/// Resolves an opaque Quilt build against the live provider, never a stale row.
pub async fn resolve_build(build_id: &str) -> Result<LoaderBuildRecord, LoaderError> {
    axial_minecraft::resolve_build_record_for_install(COMPONENT, build_id).await
}

/// Begins the retained base/child publication protocol for exactly Quilt.
///
/// The caller must retain the operation and settle every receipt or recovery.
/// `BaseCommitted` is a checkpoint, not a completed Quilt installation: activate
/// the verified base, then consume the shared loader continuation. Accepted work
/// must not be cancelled by dropping this future during publication.
pub fn install_build<'a, F>(
    operation: &'a ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    record: LoaderBuildRecord,
    send: F,
) -> impl std::future::Future<Output = Result<LoaderInstallPublicationOutcome, LoaderInstallError>> + 'a
where
    F: FnMut(DownloadProgress) + 'a,
{
    async move {
        if record.component_id != COMPONENT {
            return Err(LoaderError::InvalidBuildId.into());
        }
        axial_minecraft::install_build(operation, runtime_cache, record, send).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::loaders::{LoaderInstallSource, LoaderVersionIndex};
    use axial_minecraft::managed_path::ManagedLibraryTestAuthority;
    use axial_minecraft::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildSubjectKind, LoaderInstallStrategy,
        LoaderInstallability, LoaderSelectionMeta, LoaderSelectionReason, LoaderSelectionSource,
        LoaderTerm, LoaderTermEvidence, LoaderTermSource, build_id_for, installed_version_id_for,
        persist_loader_build_cache_fixture_for_test,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn resolve_rejects_malformed_or_other_component_identity_before_provider_io() {
        for build_id in [
            String::new(),
            "0.29.2".into(),
            "quilt-loader-0.29.2-1.21.5".into(),
            build_id_for(LoaderComponentId::Fabric, "1.21.5", "0.29.2"),
            build_id_for(LoaderComponentId::Forge, "1.21.5", "55.0.0"),
            build_id_for(LoaderComponentId::NeoForge, "1.21.5", "21.5.74"),
        ] {
            assert!(matches!(
                resolve_build(&build_id).await,
                Err(LoaderError::InvalidBuildId)
            ));
        }
    }

    #[test]
    fn cached_builds_preserve_exact_profiles_and_stability_ordering() {
        let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).expect("temporary library");
        let authority = ManagedLibraryTestAuthority::open(root.path()).expect("library authority");
        let mut beta = record(COMPONENT, "0.30.0-beta.1");
        beta.build_meta = LoaderBuildMetadata {
            terms: vec![LoaderTerm::Beta],
            evidence: vec![LoaderTermEvidence {
                term: LoaderTerm::Beta,
                source: LoaderTermSource::ExplicitVersionLabel,
            }],
            selection: LoaderSelectionMeta {
                default_rank: 100,
                reason: LoaderSelectionReason::Unstable,
                source: LoaderSelectionSource::ExplicitVersionLabel,
            },
            display_tags: vec!["Beta".into()],
        };
        let mut stable = record(COMPONENT, "0.29.2");
        stable.build_meta.selection = LoaderSelectionMeta {
            default_rank: 500,
            reason: LoaderSelectionReason::Unlabeled,
            source: LoaderSelectionSource::None,
        };
        let mut earlier_stable = record(COMPONENT, "0.29.1");
        earlier_stable.build_meta = stable.build_meta.clone();
        seed(
            authority.operation(),
            COMPONENT,
            vec![beta.clone(), earlier_stable.clone(), stable.clone()],
        );

        let (actual, state) = fetch_cached_builds(authority.operation(), "1.21.5")
            .expect("read cache")
            .expect("fresh cache");

        assert_eq!(actual, vec![stable, earlier_stable, beta]);
        assert!(state.availability.fresh);
        assert!(state.availability.cache_hit);
        assert!(!state.availability.stale);
        assert!(state.availability.last_error.is_none());
    }

    #[test]
    fn cache_is_scoped_to_quilt_and_rejects_mismatched_build_identity() {
        let other_root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).expect("other library");
        let other = ManagedLibraryTestAuthority::open(other_root.path()).expect("other authority");
        seed(
            other.operation(),
            LoaderComponentId::Fabric,
            vec![record(LoaderComponentId::Fabric, "0.16.14")],
        );
        assert!(
            fetch_cached_builds(other.operation(), "1.21.5")
                .expect("separate cache")
                .is_none()
        );

        let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).expect("Quilt library");
        let authority = ManagedLibraryTestAuthority::open(root.path()).expect("Quilt authority");
        let mut mismatched = record(COMPONENT, "0.29.2");
        mismatched.version_id = installed_version_id_for(COMPONENT, "1.21.4", "0.29.2")
            .expect("different Minecraft identity");
        seed(authority.operation(), COMPONENT, vec![mismatched]);
        assert!(matches!(
            fetch_cached_builds(authority.operation(), "1.21.5"),
            Err(LoaderError::ProviderDataInvalid { .. })
        ));
    }

    #[tokio::test]
    async fn install_rejects_other_component_and_malformed_identity_without_file_effects() {
        let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).expect("temporary library");
        let authority = ManagedLibraryTestAuthority::open(root.path()).expect("library authority");
        let runtime = ManagedRuntimeCache::isolated_for_test().expect("runtime authority");
        let before = directory_names(root.path());
        let mut invalid = record(COMPONENT, "0.29.2");
        invalid.build_id.push('!');
        let mut notifications = 0;

        for candidate in [record(LoaderComponentId::Fabric, "0.16.14"), invalid] {
            let error = install_build(authority.operation(), runtime.clone(), candidate, |_| {
                notifications += 1;
            })
            .await
            .expect_err("invalid install must fail");
            assert!(matches!(error, LoaderInstallError::Active(_)));
            assert_eq!(directory_names(root.path()), before);
            assert!(!root.path().join("versions").exists());
        }
        assert_eq!(notifications, 0);
    }

    fn record(component_id: LoaderComponentId, loader_version: &str) -> LoaderBuildRecord {
        let minecraft_version = "1.21.5";
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().into(),
            build_id: build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version: minecraft_version.into(),
            loader_version: loader_version.into(),
            version_id: installed_version_id_for(component_id, minecraft_version, loader_version)
                .expect("installed identity"),
            build_meta: LoaderBuildMetadata::default(),
            strategy: if component_id == COMPONENT {
                LoaderInstallStrategy::QuiltProfile
            } else {
                LoaderInstallStrategy::FabricProfile
            },
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: if component_id == COMPONENT {
                    format!(
                        "https://meta.quiltmc.org/v3/versions/loader/{minecraft_version}/{loader_version}/profile/json"
                    )
                } else {
                    format!(
                        "https://meta.fabricmc.net/v2/versions/loader/{minecraft_version}/{loader_version}/profile/json"
                    )
                },
            },
        }
    }

    fn seed(
        operation: &ManagedLibraryOperation,
        component_id: LoaderComponentId,
        builds: Vec<LoaderBuildRecord>,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64;
        persist_loader_build_cache_fixture_for_test(
            operation,
            "1.21.5",
            &LoaderVersionIndex {
                component_id,
                builds,
            },
            now,
        )
        .expect("persist provider cache");
    }

    fn directory_names(path: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut names = std::fs::read_dir(path)
            .expect("inspect library")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}
