use super::cache::{resolve_cached, resolve_fresh_cached};
use super::normalize::{normalize_build_index, normalize_supported_versions};
use crate::loaders::api::{
    loader_components, parse_build_id, validate_loader_build_record_identity,
};
use crate::loaders::providers;
#[cfg(feature = "test-support")]
use crate::loaders::types::{CachedCatalog, LOADER_CATALOG_SCHEMA_VERSION};
use crate::loaders::types::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderComponentRecord, LoaderError,
    LoaderGameVersion, LoaderProviderFailureKind, LoaderVersionIndex,
};
use crate::managed_fs::ManagedLibraryOperation;
use crate::manifest::{
    FetchError as ManifestError, VersionManifest, fetch_version_manifest_cached_cancellable,
};
use crate::portable_path::PortableFileName;
use crate::version_meta::{enrich_loader_game_versions, manifest_release_entries};
use futures_util::FutureExt;
use std::collections::HashMap;
use std::time::Duration;

const SUPPORTED_VERSIONS_TTL: Duration = Duration::from_secs(60 * 60);
const BUILD_INDEX_TTL: Duration = Duration::from_secs(30 * 60);

pub fn fetch_components() -> Vec<LoaderComponentRecord> {
    loader_components()
}

pub async fn fetch_supported_versions(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    fetch_supported_versions_cancellable(operation, component_id, std::future::pending()).await
}

pub async fn fetch_supported_versions_cancellable(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    let cancelled = cancelled.shared();
    fetch_supported_versions_with(
        operation,
        component_id,
        cancelled.clone(),
        providers::fetch_supported_versions(component_id),
        fetch_version_manifest_cached_cancellable(operation, cancelled),
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn fetch_fabric_game_versions_for_test(
    operation: &ManagedLibraryOperation,
    url: &reqwest::Url,
    manifest_url: Option<&reqwest::Url>,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    crate::loaders::http::validate_loopback_url_for_test(url)?;
    let cancelled = cancelled.shared();
    let version_manifest = async {
        match manifest_url {
            Some(url) => {
                crate::manifest::fetch_version_manifest_cached_from_loopback_for_test(
                    operation,
                    url,
                    cancelled.clone(),
                )
                .await
            }
            None => fetch_version_manifest_cached_cancellable(operation, cancelled.clone()).await,
        }
    };
    fetch_supported_versions_with(
        operation,
        LoaderComponentId::Fabric,
        cancelled.clone(),
        providers::fetch_game_versions_from_loopback_for_test(url),
        version_manifest,
    )
    .await
}

async fn fetch_supported_versions_with(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    cancelled: impl std::future::Future<Output = ()>,
    fetch_live: impl std::future::Future<Output = Result<Vec<LoaderGameVersion>, LoaderError>>,
    version_manifest: impl std::future::Future<Output = Result<VersionManifest, ManifestError>>,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    let supported_versions = resolve_cached(
        operation,
        supported_versions_cache_name(component_id)?,
        SUPPORTED_VERSIONS_TTL,
        || async {
            tokio::select! {
                biased;
                result = fetch_live => result,
                _ = cancelled => Err(LoaderError::Cancelled),
            }
        },
    );
    let (supported_versions, version_manifest) = tokio::join!(supported_versions, version_manifest);

    let (mut versions, catalog) = supported_versions?;
    let catalog_order = match version_manifest {
        Ok(manifest) => {
            let releases = manifest_release_entries(&manifest.versions);
            enrich_loader_game_versions(&mut versions, &manifest.versions, &releases);
            Some(catalog_version_order(&manifest.versions))
        }
        Err(ManifestError::Cancelled) => return Err(LoaderError::Cancelled),
        Err(ManifestError::Unavailable(_)) => {
            enrich_loader_game_versions(&mut versions, &[], &[]);
            None
        }
    };
    Ok((
        normalize_supported_versions(versions, catalog_order.as_ref()),
        catalog,
    ))
}

pub async fn fetch_builds(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    fetch_builds_cancellable(
        operation,
        component_id,
        minecraft_version,
        std::future::pending(),
    )
    .await
}

/// Cancellation applies only to acquisition; a received result settles its cache work.
pub async fn fetch_builds_cancellable(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    minecraft_version: &str,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    fetch_builds_with(
        operation,
        component_id,
        minecraft_version,
        cancelled,
        |minecraft_version| async move {
            providers::fetch_build_index(component_id, &minecraft_version).await
        },
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn fetch_fabric_builds_for_test(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
    url: &reqwest::Url,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    crate::loaders::http::validate_loopback_url_for_test(url)?;
    fetch_builds_with(
        operation,
        LoaderComponentId::Fabric,
        minecraft_version,
        cancelled,
        |minecraft_version| async move {
            providers::fetch_builds_from_loopback_for_test(&minecraft_version, url).await
        },
    )
    .await
}

async fn fetch_builds_with<F, Fut>(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    minecraft_version: &str,
    cancelled: impl std::future::Future<Output = ()>,
    fetch_live: F,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<LoaderVersionIndex, LoaderError>>,
{
    let minecraft_version = sanitize_segment(minecraft_version)?;
    let (index, catalog) = resolve_cached(
        operation,
        build_index_cache_name(component_id, &minecraft_version)?,
        BUILD_INDEX_TTL,
        || async {
            tokio::select! {
                biased;
                result = fetch_live(minecraft_version.clone()) => result,
                _ = cancelled => Err(LoaderError::Cancelled),
            }
        },
    )
    .await?;
    let normalized = normalize_build_index(index);
    validate_build_index_identity(&normalized, component_id, &minecraft_version)?;
    Ok((normalized.builds, catalog))
}

pub fn fetch_cached_builds(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<Option<(Vec<LoaderBuildRecord>, LoaderCatalogState)>, LoaderError> {
    let minecraft_version = sanitize_segment(minecraft_version)?;
    let Some((index, catalog)) = resolve_fresh_cached(
        operation,
        build_index_cache_name(component_id, &minecraft_version)?,
        BUILD_INDEX_TTL,
    ) else {
        return Ok(None);
    };
    let normalized = normalize_build_index(index);
    validate_build_index_identity(&normalized, component_id, &minecraft_version)?;
    Ok(Some((normalized.builds, catalog)))
}

fn validate_build_index_identity(
    index: &LoaderVersionIndex,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<(), LoaderError> {
    let valid = index.component_id == component_id
        && index.builds.iter().all(|record| {
            record.component_id == component_id
                && record.minecraft_version == minecraft_version
                && validate_loader_build_record_identity(record).is_ok()
        });
    if valid {
        Ok(())
    } else {
        Err(LoaderError::ProviderDataInvalid {
            kind: LoaderProviderFailureKind::SchemaInvalid,
            status: None,
        })
    }
}

pub async fn resolve_build_record_for_install(
    component_id: LoaderComponentId,
    build_id: &str,
) -> Result<LoaderBuildRecord, LoaderError> {
    resolve_build_record_for_install_cancellable(component_id, build_id, std::future::pending())
        .await
}

pub async fn resolve_build_record_for_install_cancellable(
    component_id: LoaderComponentId,
    build_id: &str,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<LoaderBuildRecord, LoaderError> {
    resolve_build_record_for_install_with(
        component_id,
        build_id,
        cancelled,
        |minecraft_version| async move {
            providers::fetch_build_index(component_id, &minecraft_version).await
        },
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn resolve_fabric_build_for_test(
    build_id: &str,
    url: &reqwest::Url,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<LoaderBuildRecord, LoaderError> {
    crate::loaders::http::validate_loopback_url_for_test(url)?;
    resolve_build_record_for_install_with(
        LoaderComponentId::Fabric,
        build_id,
        cancelled,
        |minecraft_version| async move {
            providers::fetch_builds_from_loopback_for_test(&minecraft_version, url).await
        },
    )
    .await
}

async fn resolve_build_record_for_install_with<F, Fut>(
    component_id: LoaderComponentId,
    build_id: &str,
    cancelled: impl std::future::Future<Output = ()>,
    fetch_live: F,
) -> Result<LoaderBuildRecord, LoaderError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<LoaderVersionIndex, LoaderError>>,
{
    let Some((parsed_component_id, minecraft_version, _loader_version)) = parse_build_id(build_id)
    else {
        return Err(LoaderError::InvalidBuildId);
    };
    if parsed_component_id != component_id {
        return Err(LoaderError::InvalidBuildId);
    }
    let minecraft_version = sanitize_segment(&minecraft_version)?;

    let live = tokio::select! {
        biased;
        result = fetch_live(minecraft_version.clone()) => result,
        _ = cancelled => Err(LoaderError::Cancelled),
    }?;
    let normalized = normalize_build_index(live);
    validate_build_index_identity(&normalized, component_id, &minecraft_version)?;
    resolve_live_build_record(component_id, build_id, normalized.builds)
}

fn resolve_live_build_record(
    component_id: LoaderComponentId,
    build_id: &str,
    builds: Vec<LoaderBuildRecord>,
) -> Result<LoaderBuildRecord, LoaderError> {
    builds
        .into_iter()
        .find(|build| build.component_id == component_id && build.build_id == build_id)
        .ok_or_else(|| {
            LoaderError::BuildNotFound(format!("{} build {}", component_id.short_key(), build_id,))
        })
}

fn supported_versions_cache_name(
    component_id: LoaderComponentId,
) -> Result<PortableFileName, LoaderError> {
    catalog_cache_name(format!(
        "component-{}-supported-versions.json",
        component_id.short_key()
    ))
}

fn build_index_cache_name(
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<PortableFileName, LoaderError> {
    catalog_cache_name(format!(
        "component-{}-builds-{}.json",
        component_id.short_key(),
        minecraft_version
    ))
}

fn catalog_cache_name(name: String) -> Result<PortableFileName, LoaderError> {
    PortableFileName::new_exact(&name)
        .map_err(|_| LoaderError::Verify("loader catalog cache name is invalid".to_string()))
}

#[cfg(feature = "test-support")]
pub fn persist_loader_build_cache_fixture_for_test(
    operation: &ManagedLibraryOperation,
    minecraft_version: &str,
    index: &LoaderVersionIndex,
    fetched_at_ms: i64,
) -> Result<(), LoaderError> {
    let minecraft_version = sanitize_segment(minecraft_version)?;
    let name = build_index_cache_name(index.component_id, &minecraft_version)?;
    super::cache::write_cache_fixture(
        operation,
        &name,
        &CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms,
            value: index,
        },
    )
}

#[cfg(feature = "test-support")]
pub fn persist_loader_supported_versions_cache_fixture_for_test(
    operation: &ManagedLibraryOperation,
    component_id: LoaderComponentId,
    versions: &[LoaderGameVersion],
    fetched_at_ms: i64,
) -> Result<(), LoaderError> {
    let name = supported_versions_cache_name(component_id)?;
    super::cache::write_cache_fixture(
        operation,
        &name,
        &CachedCatalog {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
            fetched_at_ms,
            value: versions,
        },
    )
}

fn sanitize_segment(value: &str) -> Result<String, LoaderError> {
    let value = value.trim();
    if value.is_empty()
        || value.contains("..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(LoaderError::InvalidMinecraftVersion);
    }
    Ok(value.to_string())
}

fn catalog_version_order(entries: &[crate::manifest::ManifestEntry]) -> HashMap<String, usize> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.clone(), index))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        fetch_builds_with, fetch_cached_builds, resolve_build_record_for_install_with,
        validate_build_index_identity,
    };
    use crate::loaders::types::{
        CachedCatalog, LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord,
        LoaderBuildSubjectKind, LoaderComponentId, LoaderError, LoaderInstallSource,
        LoaderInstallStrategy, LoaderInstallability, LoaderProviderFailureKind, LoaderVersionIndex,
    };
    use crate::loaders::{build_id_for, installed_version_id_for};
    use crate::managed_fs::ManagedLibraryRoot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn supported_version_acquisition_cancellation_preserves_absent_or_stale_catalog() {
        for stale in [false, true] {
            let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
            let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
            let operation = root.try_acquire().unwrap();
            operation.prepare_layout().unwrap();
            seed_manifest(&operation);
            let component = LoaderComponentId::Fabric;
            if stale {
                super::persist_loader_supported_versions_cache_fixture_for_test(
                    &operation,
                    component,
                    &[game_version()],
                    1,
                )
                .unwrap();
            }
            let path = temporary
                .path()
                .join("cache/loaders/catalog/component-fabric-supported-versions.json");
            let original = stale.then(|| std::fs::read(&path).unwrap());

            let error = tokio::time::timeout(
                Duration::from_secs(2),
                super::fetch_supported_versions_with(
                    &operation,
                    component,
                    std::future::ready(()),
                    std::future::pending(),
                    super::fetch_version_manifest_cached_cancellable(
                        &operation,
                        std::future::ready(()),
                    ),
                ),
            )
            .await
            .expect("pending acquisition must be cancellable")
            .expect_err("cancellation must not serve stale catalog");

            assert!(matches!(error, LoaderError::Cancelled));
            if let Some(original) = original {
                assert_eq!(std::fs::read(&path).unwrap(), original);
            } else {
                assert!(!path.exists());
            }
        }
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn supported_version_acquisition_failure_wins_over_cancellation() {
        let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
        let operation = root.try_acquire().unwrap();
        operation.prepare_layout().unwrap();
        seed_manifest(&operation);

        let error = super::fetch_supported_versions_with(
            &operation,
            LoaderComponentId::Fabric,
            std::future::ready(()),
            std::future::ready(Err(LoaderError::ProviderUnavailable {
                kind: LoaderProviderFailureKind::HttpServer,
                status: Some(503),
            })),
            super::fetch_version_manifest_cached_cancellable(&operation, std::future::ready(())),
        )
        .await
        .expect_err("completed provider refusal");

        assert!(matches!(
            error,
            LoaderError::CatalogUnavailable {
                provider_failure_kind: Some(LoaderProviderFailureKind::HttpServer),
                provider_status: Some(503),
                ..
            }
        ));
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn supported_version_acquisition_publishes_and_enriches_despite_cancellation() {
        let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
        let operation = root.try_acquire().unwrap();
        operation.prepare_layout().unwrap();
        seed_manifest(&operation);
        let raw = vec![game_version()];

        let (versions, state) = super::fetch_supported_versions_with(
            &operation,
            LoaderComponentId::Fabric,
            std::future::ready(()),
            std::future::ready(Ok(raw.clone())),
            super::fetch_version_manifest_cached_cancellable(&operation, std::future::ready(())),
        )
        .await
        .expect("successful acquisition must settle publication");
        let persisted: CachedCatalog<Vec<super::LoaderGameVersion>> = serde_json::from_slice(
            &std::fs::read(
                temporary
                    .path()
                    .join("cache/loaders/catalog/component-fabric-supported-versions.json"),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(persisted.value, raw);
        assert!(state.availability.fresh);
        assert!(!state.availability.cache_hit);
        assert_eq!(state.availability.last_error, None);
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id, "1.21.5");
        assert_eq!(versions[0].release_time, "2025-03-25T12:00:00+00:00");
        assert_eq!(versions[0].stable_hint, Some(true));
    }

    #[cfg(feature = "test-support")]
    fn game_version() -> super::LoaderGameVersion {
        serde_json::from_str(r#"{"id":"1.21.5","stable_hint":true}"#).unwrap()
    }

    #[cfg(feature = "test-support")]
    fn seed_manifest(operation: &crate::managed_fs::ManagedLibraryOperation) {
        crate::manifest::persist_version_manifest_cache_fixture_for_test(
            operation,
            br#"{"latest":{"release":"1.21.5","snapshot":"1.21.5"},"versions":[{"id":"1.21.5","type":"release","url":"https://piston-meta.mojang.com/version.json","sha1":"0123456789012345678901234567890123456789","releaseTime":"2025-03-25T12:00:00+00:00"}]}"#,
        ).unwrap();
    }

    #[tokio::test]
    async fn build_acquisition_cancellation_leaves_no_catalog() {
        let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
        let operation = root.try_acquire().unwrap();
        operation.prepare_layout().unwrap();

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            fetch_builds_with(
                &operation,
                LoaderComponentId::Fabric,
                "1.21.5",
                std::future::ready(()),
                |_| std::future::pending(),
            ),
        )
        .await
        .expect("pending acquisition must be cancellable")
        .expect_err("cancelled acquisition");

        assert!(matches!(error, LoaderError::Cancelled));
        assert_eq!(error.availability_failure_kind(), None);
        assert!(
            fetch_cached_builds(&operation, LoaderComponentId::Fabric, "1.21.5")
                .unwrap()
                .is_none()
        );
        assert!(
            !temporary
                .path()
                .join("cache/loaders/catalog/component-fabric-builds-1.21.5.json")
                .exists()
        );
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn build_acquisition_cancellation_preserves_stale_catalog() {
        let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
        let operation = root.try_acquire().unwrap();
        operation.prepare_layout().unwrap();
        let component = LoaderComponentId::Fabric;
        let build_id = build_id_for(component, "1.21.5", "0.16.14");
        super::persist_loader_build_cache_fixture_for_test(
            &operation,
            "1.21.5",
            &LoaderVersionIndex {
                component_id: component,
                builds: vec![build_record(component, &build_id)],
            },
            1,
        )
        .unwrap();
        let path = temporary
            .path()
            .join("cache/loaders/catalog/component-fabric-builds-1.21.5.json");
        let original = std::fs::read(&path).unwrap();

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            fetch_builds_with(
                &operation,
                component,
                "1.21.5",
                std::future::ready(()),
                |_| std::future::pending(),
            ),
        )
        .await
        .expect("pending acquisition must be cancellable")
        .expect_err("cancellation must not serve stale catalog");

        assert!(matches!(error, LoaderError::Cancelled));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn completed_build_acquisition_failure_wins_over_cancellation() {
        let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
        let operation = root.try_acquire().unwrap();
        operation.prepare_layout().unwrap();

        let error = fetch_builds_with(
            &operation,
            LoaderComponentId::Fabric,
            "1.21.5",
            std::future::ready(()),
            |_| {
                std::future::ready(Err(LoaderError::ProviderUnavailable {
                    kind: LoaderProviderFailureKind::HttpServer,
                    status: Some(503),
                }))
            },
        )
        .await
        .expect_err("completed provider refusal");

        assert!(matches!(
            error,
            LoaderError::CatalogUnavailable {
                provider_failure_kind: Some(LoaderProviderFailureKind::HttpServer),
                provider_status: Some(503),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn completed_build_acquisition_publishes_cache_despite_cancellation() {
        let temporary = tempfile::tempdir_in(crate::test_temp_root()).unwrap();
        let root = ManagedLibraryRoot::open_for_test(temporary.path()).unwrap();
        let operation = root.try_acquire().unwrap();
        operation.prepare_layout().unwrap();
        let component = LoaderComponentId::Fabric;
        let build_id = build_id_for(component, "1.21.5", "0.16.14");
        let index = LoaderVersionIndex {
            component_id: component,
            builds: vec![build_record(component, &build_id)],
        };

        let (builds, state) = fetch_builds_with(
            &operation,
            component,
            "1.21.5",
            std::future::ready(()),
            |_| std::future::ready(Ok(index.clone())),
        )
        .await
        .expect("successful acquisition must settle publication");
        let persisted: CachedCatalog<LoaderVersionIndex> = serde_json::from_slice(
            &std::fs::read(
                temporary
                    .path()
                    .join("cache/loaders/catalog/component-fabric-builds-1.21.5.json"),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(builds, index.builds);
        assert!(state.availability.fresh);
        assert!(!state.availability.cache_hit);
        assert_eq!(state.availability.last_error, None);
        assert_eq!(persisted.value, index);
        assert_eq!(
            fetch_cached_builds(&operation, component, "1.21.5")
                .unwrap()
                .expect("published catalog")
                .0,
            builds
        );
    }

    #[test]
    fn catalog_rejects_noncanonical_installed_version_id() {
        let component_id = LoaderComponentId::Fabric;
        let build_id = build_id_for(component_id, "1.21.5", "0.16.14");
        let mut record = build_record(component_id, &build_id);
        record.version_id = "fabric-loader-0.16.14-1.21.5".to_string();

        let error = validate_build_index_identity(
            &LoaderVersionIndex {
                component_id,
                builds: vec![record],
            },
            component_id,
            "1.21.5",
        )
        .expect_err("noncanonical provider identity");

        assert!(matches!(
            error,
            LoaderError::ProviderDataInvalid {
                kind: LoaderProviderFailureKind::SchemaInvalid,
                status: None,
            }
        ));
    }

    #[tokio::test]
    async fn install_resolution_cancels_pending_provider_acquisition() {
        let component = LoaderComponentId::Fabric;
        let build_id = build_id_for(component, "1.21.5", "0.16.14");
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            resolve_build_record_for_install_with(
                component,
                &build_id,
                std::future::ready(()),
                |_| std::future::pending(),
            ),
        )
        .await
        .expect("pending acquisition must be cancellable")
        .expect_err("cancelled acquisition");

        assert!(matches!(error, LoaderError::Cancelled));
    }

    #[tokio::test]
    async fn install_resolution_completed_provider_result_wins_over_cancellation() {
        let component = LoaderComponentId::Fabric;
        let build_id = build_id_for(component, "1.21.5", "0.16.14");
        let expected = build_record(component, &build_id);
        let resolved = resolve_build_record_for_install_with(
            component,
            &build_id,
            std::future::ready(()),
            |_| {
                std::future::ready(Ok(LoaderVersionIndex {
                    component_id: component,
                    builds: vec![expected.clone()],
                }))
            },
        )
        .await
        .expect("completed provider result");

        assert_eq!(resolved, expected);
    }

    #[tokio::test]
    async fn install_resolution_completed_provider_error_wins_over_cancellation() {
        let component = LoaderComponentId::Fabric;
        let build_id = build_id_for(component, "1.21.5", "0.16.14");
        let error = resolve_build_record_for_install_with(
            component,
            &build_id,
            std::future::ready(()),
            |_| {
                std::future::ready(Err(LoaderError::ProviderUnavailable {
                    kind: LoaderProviderFailureKind::HttpServer,
                    status: Some(503),
                }))
            },
        )
        .await
        .expect_err("completed provider refusal");

        assert!(matches!(
            error,
            LoaderError::ProviderUnavailable {
                kind: LoaderProviderFailureKind::HttpServer,
                status: Some(503),
            }
        ));
    }

    #[tokio::test]
    async fn install_resolution_always_fetches_the_live_provider_index() {
        let component_id = LoaderComponentId::Fabric;
        let build_id = build_id_for(component_id, "1.21.5", "0.16.14");
        let live_build_id = build_id.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::clone(&calls);

        let resolved = resolve_build_record_for_install_with(
            component_id,
            &build_id,
            std::future::pending(),
            move |minecraft_version| {
                assert_eq!(minecraft_version, "1.21.5");
                fetch_calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(LoaderVersionIndex {
                    component_id,
                    builds: vec![build_record(component_id, &live_build_id)],
                }))
            },
        )
        .await
        .expect("live build");

        assert_eq!(resolved.build_id, build_id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn install_resolution_rejects_build_absent_from_live_compatibility_catalog() {
        let component_id = LoaderComponentId::Fabric;
        let build_id = build_id_for(component_id, "26.2", "0.19.3");

        let error = resolve_build_record_for_install_with(
            component_id,
            &build_id,
            std::future::pending(),
            move |minecraft_version| {
                assert_eq!(minecraft_version, "26.2");
                std::future::ready(Ok(LoaderVersionIndex {
                    component_id,
                    builds: Vec::new(),
                }))
            },
        )
        .await
        .expect_err("provider-filtered build");

        assert!(matches!(error, LoaderError::BuildNotFound(_)));
    }

    #[tokio::test]
    async fn install_resolution_rejects_unsafe_version_before_provider_fetch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::clone(&calls);
        let build_id = build_id_for(LoaderComponentId::Fabric, "..", "0.16.14");

        let error = resolve_build_record_for_install_with(
            LoaderComponentId::Fabric,
            &build_id,
            std::future::ready(()),
            move |_| {
                fetch_calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(LoaderVersionIndex {
                    component_id: LoaderComponentId::Fabric,
                    builds: Vec::new(),
                }))
            },
        )
        .await
        .expect_err("unsafe version segment");

        assert!(matches!(error, LoaderError::InvalidMinecraftVersion));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    fn build_record(component_id: LoaderComponentId, build_id: &str) -> LoaderBuildRecord {
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: build_id.to_string(),
            minecraft_version: "1.21.5".to_string(),
            loader_version: "0.16.14".to_string(),
            version_id: installed_version_id_for(component_id, "1.21.5", "0.16.14")
                .expect("canonical installed version id"),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://example.invalid/profile.json".to_string(),
            },
        }
    }
}
