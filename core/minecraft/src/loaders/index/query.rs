use super::cache::{resolve_cached, resolve_fresh_cached};
use super::normalize::{normalize_build_index, normalize_supported_versions};
use crate::loaders::api::{
    loader_components, parse_build_id, validate_loader_build_record_identity,
};
use crate::loaders::providers;
use crate::loaders::types::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderComponentRecord, LoaderError,
    LoaderGameVersion, LoaderProviderFailureKind, LoaderVersionIndex,
};
use crate::manifest::fetch_version_manifest_cached;
use crate::paths::loader_catalog_dir;
use crate::version_meta::{enrich_loader_game_versions, manifest_release_entries};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SUPPORTED_VERSIONS_TTL: Duration = Duration::from_secs(60 * 60);
const BUILD_INDEX_TTL: Duration = Duration::from_secs(30 * 60);

pub fn fetch_components() -> Vec<LoaderComponentRecord> {
    loader_components()
}

pub async fn fetch_supported_versions(
    library_dir: &Path,
    component_id: LoaderComponentId,
) -> Result<(Vec<LoaderGameVersion>, LoaderCatalogState), LoaderError> {
    let supported_versions = resolve_cached(
        supported_versions_cache_path(library_dir, component_id),
        SUPPORTED_VERSIONS_TTL,
        || providers::fetch_supported_versions(component_id),
    );
    let version_manifest = fetch_version_manifest_cached(library_dir);
    let (supported_versions, version_manifest) = tokio::join!(supported_versions, version_manifest);

    let (mut versions, catalog) = supported_versions?;
    let catalog_order = match version_manifest {
        Ok(manifest) => {
            let releases = manifest_release_entries(&manifest.versions);
            enrich_loader_game_versions(&mut versions, &manifest.versions, &releases);
            Some(catalog_version_order(&manifest.versions))
        }
        Err(_) => {
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
    library_dir: &Path,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<(Vec<LoaderBuildRecord>, LoaderCatalogState), LoaderError> {
    let minecraft_version = sanitize_segment(minecraft_version)?;
    let (index, catalog) = resolve_cached(
        build_index_cache_path(library_dir, component_id, &minecraft_version),
        BUILD_INDEX_TTL,
        || providers::fetch_build_index(component_id, &minecraft_version),
    )
    .await?;
    let normalized = normalize_build_index(index);
    validate_build_index_identity(&normalized, component_id, &minecraft_version)?;
    Ok((normalized.builds, catalog))
}

pub fn fetch_cached_builds(
    library_dir: &Path,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<Option<(Vec<LoaderBuildRecord>, LoaderCatalogState)>, LoaderError> {
    let minecraft_version = sanitize_segment(minecraft_version)?;
    let Some((index, catalog)) = resolve_fresh_cached(
        build_index_cache_path(library_dir, component_id, &minecraft_version),
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
    resolve_build_record_for_install_with(component_id, build_id, |minecraft_version| async move {
        providers::fetch_build_index(component_id, &minecraft_version).await
    })
    .await
}

async fn resolve_build_record_for_install_with<F, Fut>(
    component_id: LoaderComponentId,
    build_id: &str,
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

    let normalized = normalize_build_index(fetch_live(minecraft_version.clone()).await?);
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

fn supported_versions_cache_path(library_dir: &Path, component_id: LoaderComponentId) -> PathBuf {
    loader_catalog_dir(library_dir).join(format!(
        "component-{}-supported-versions.json",
        component_id.short_key()
    ))
}

fn build_index_cache_path(
    library_dir: &Path,
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> PathBuf {
    loader_catalog_dir(library_dir).join(format!(
        "component-{}-builds-{}.json",
        component_id.short_key(),
        minecraft_version
    ))
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
    use super::{resolve_build_record_for_install_with, validate_build_index_identity};
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderError, LoaderInstallSource, LoaderInstallStrategy,
        LoaderInstallability, LoaderProviderFailureKind, LoaderVersionIndex,
    };
    use crate::loaders::{build_id_for, installed_version_id_for};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    async fn install_resolution_always_fetches_the_live_provider_index() {
        let component_id = LoaderComponentId::Fabric;
        let build_id = build_id_for(component_id, "1.21.5", "0.16.14");
        let live_build_id = build_id.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::clone(&calls);

        let resolved = resolve_build_record_for_install_with(
            component_id,
            &build_id,
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
