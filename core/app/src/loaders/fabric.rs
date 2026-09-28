//! Fabric catalog and installation entrypoints.
//!
//! Keep provider compatibility, historical profile parsing, artifact verification,
//! and publication in the retained Minecraft leaf. This boundary accepts an
//! admitted library operation, never a caller-authored destination path.

use axial_minecraft::loaders::{
    self, LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderError, LoaderGameVersion,
    LoaderInstallError, LoaderInstallPublicationOutcome,
};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{DownloadProgress, ManagedRuntimeCache};

pub const COMPONENT: LoaderComponentId = LoaderComponentId::Fabric;

/// Includes the provider's supported releases and snapshots, with Minecraft
/// ordering and the retained freshness/offline-cache projection.
pub async fn supported_versions(
    library: &ManagedLibraryOperation,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    loaders::fetch_supported_versions(library, COMPONENT).await
}

/// The provider only admits complete compatibility rows: exact loader Maven
/// coordinate, exact intermediary identity, and a nonempty client main class.
/// Both historical string and current environment-map main classes are retained.
pub async fn builds(
    library: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    loaders::fetch_builds(library, COMPONENT, minecraft_version).await
}

/// Reads a fresh catalog without network I/O. A missing/expired cache stays
/// unavailable; cached selections never authorize installation by themselves.
pub fn cached_builds(
    library: &ManagedLibraryOperation,
    minecraft_version: &str,
) -> Result<Option<(Vec<LoaderBuildRecord>, LoaderCatalogState)>, LoaderError> {
    loaders::fetch_cached_builds(library, COMPONENT, minecraft_version)
}

/// Resolves an opaque Fabric build identity against fresh provider compatibility.
/// An incomplete, removed, or cross-component build cannot be selected.
pub async fn resolve(build_id: &str) -> Result<LoaderBuildRecord, LoaderError> {
    loaders::resolve_build_record_for_install(COMPONENT, build_id).await
}

/// Starts the exact catalog selection, revalidating it against live provider
/// authority before effects. A base commit is an intermediate outcome, not a
/// successful Fabric installation. The install owner must verify, activate, and
/// acknowledge that commit, then pass its retained continuation to the shared
/// installer primitive. Publication failures retain their recovery authority.
pub async fn start<F>(
    library: &ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    record: LoaderBuildRecord,
    send: F,
) -> Result<LoaderInstallPublicationOutcome, LoaderInstallError>
where
    F: FnMut(DownloadProgress) + Send,
{
    if record.component_id != COMPONENT {
        return Err(LoaderError::InvalidBuildId.into());
    }
    loaders::install_build(library, runtime_cache, record, send).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::loaders::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildSubjectKind, LoaderInstallSource,
        LoaderInstallStrategy, LoaderInstallability, LoaderVersionIndex, build_id_for,
        installed_version_id_for, persist_loader_build_cache_fixture_for_test,
    };
    use axial_minecraft::managed_path::ManagedLibraryTestAuthority;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn record(minecraft: &str, loader: &str) -> LoaderBuildRecord {
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id: COMPONENT,
            component_name: "Fabric".into(),
            build_id: build_id_for(COMPONENT, minecraft, loader),
            minecraft_version: minecraft.into(),
            loader_version: loader.into(),
            version_id: installed_version_id_for(COMPONENT, minecraft, loader).unwrap(),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: format!(
                    "https://meta.fabricmc.net/v2/versions/loader/{minecraft}/{loader}/profile/json"
                ),
            },
        }
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[tokio::test]
    async fn catalog_cache_preserves_exact_historical_and_current_builds() {
        let directory = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = ManagedLibraryTestAuthority::open(directory.path()).unwrap();
        for (minecraft, loader) in [("1.14", "0.2.0.71"), ("26.1", "0.19.3")] {
            let expected = record(minecraft, loader);
            persist_loader_build_cache_fixture_for_test(
                library.operation(),
                minecraft,
                &LoaderVersionIndex {
                    component_id: COMPONENT,
                    builds: vec![expected.clone()],
                },
                now_ms(),
            )
            .unwrap();

            let (actual, state) = builds(library.operation(), minecraft).await.unwrap();
            assert_eq!(actual, vec![expected]);
            assert!(state.availability.cache_hit);
            assert!(state.availability.fresh);
            assert!(!state.availability.stale);
        }
    }

    #[test]
    fn absent_or_expired_catalog_is_not_a_selectable_empty_success() {
        let directory = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = ManagedLibraryTestAuthority::open(directory.path()).unwrap();
        assert!(
            cached_builds(library.operation(), "1.14")
                .unwrap()
                .is_none()
        );
        persist_loader_build_cache_fixture_for_test(
            library.operation(),
            "1.14",
            &LoaderVersionIndex {
                component_id: COMPONENT,
                builds: vec![record("1.14", "0.2.0.71")],
            },
            0,
        )
        .unwrap();
        assert!(
            cached_builds(library.operation(), "1.14")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cached_record_with_wrong_minecraft_identity_is_unavailable() {
        let directory = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = ManagedLibraryTestAuthority::open(directory.path()).unwrap();
        persist_loader_build_cache_fixture_for_test(
            library.operation(),
            "1.14",
            &LoaderVersionIndex {
                component_id: COMPONENT,
                builds: vec![record("26.1", "0.19.3")],
            },
            now_ms(),
        )
        .unwrap();
        assert!(matches!(
            cached_builds(library.operation(), "1.14"),
            Err(LoaderError::ProviderDataInvalid { .. })
        ));
    }

    #[tokio::test]
    async fn start_rejects_cross_component_or_forged_identity_before_effects() {
        let directory = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let sentinel = directory.path().join("user-file");
        std::fs::write(&sentinel, b"preserve").unwrap();
        let library = ManagedLibraryTestAuthority::open(directory.path()).unwrap();
        let runtime = ManagedRuntimeCache::isolated_for_test().unwrap();
        let progress_calls = AtomicUsize::new(0);
        let before = std::fs::read_dir(directory.path()).unwrap().count();
        let mut wrong_component = record("1.14", "0.2.0.71");
        wrong_component.component_id = LoaderComponentId::Quilt;
        let mut forged_identity = record("1.14", "0.2.0.71");
        forged_identity.build_id.push('A');

        for invalid in [wrong_component, forged_identity] {
            start(library.operation(), runtime.clone(), invalid, |_| {
                progress_calls.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .expect_err("invalid selection must fail before provider or installation I/O");
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), before);
        }
        assert_eq!(progress_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolving_a_different_loader_does_not_fall_back_to_fabric() {
        for component in [
            LoaderComponentId::Quilt,
            LoaderComponentId::Forge,
            LoaderComponentId::NeoForge,
        ] {
            let id = build_id_for(component, "1.20.1", "0.16.14");
            assert!(matches!(
                resolve(&id).await,
                Err(LoaderError::InvalidBuildId)
            ));
        }
    }
}
