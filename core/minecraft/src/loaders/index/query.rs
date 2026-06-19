use super::cache::{resolve_cached, resolve_fresh_cached};
use super::normalize::{normalize_build_index, normalize_supported_versions};
use crate::loaders::api::{loader_components, parse_build_id};
use crate::loaders::providers;
use crate::loaders::types::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderComponentRecord, LoaderError,
    LoaderGameVersion,
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
    Ok(Some((normalized.builds, catalog)))
}

pub async fn resolve_build_record(
    library_dir: &Path,
    component_id: LoaderComponentId,
    build_id: &str,
) -> Result<LoaderBuildRecord, LoaderError> {
    let Some((parsed_component_id, minecraft_version, _loader_version)) = parse_build_id(build_id)
    else {
        return Err(LoaderError::InvalidBuildId);
    };
    if parsed_component_id != component_id {
        return Err(LoaderError::InvalidBuildId);
    }

    let (builds, catalog) = fetch_builds(library_dir, component_id, &minecraft_version).await?;
    resolve_build_record_from_catalog(component_id, build_id, builds, &catalog)
}

fn resolve_build_record_from_catalog(
    component_id: LoaderComponentId,
    build_id: &str,
    builds: Vec<LoaderBuildRecord>,
    catalog: &LoaderCatalogState,
) -> Result<LoaderBuildRecord, LoaderError> {
    if catalog.availability.stale || !catalog.availability.fresh {
        return Err(LoaderError::CatalogStale);
    }

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
    use super::resolve_build_record_from_catalog;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderAvailability, LoaderBuildMetadata, LoaderBuildRecord,
        LoaderBuildSubjectKind, LoaderCatalogState, LoaderComponentId, LoaderError,
        LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    };

    #[test]
    fn exact_build_resolver_rejects_stale_catalogs() {
        let component_id = LoaderComponentId::Fabric;
        let build_id = "fabric:1.21.5:0.16.14";

        let error = resolve_build_record_from_catalog(
            component_id,
            build_id,
            vec![build_record(component_id, build_id)],
            &catalog_state(false, true),
        )
        .expect_err("stale catalog should be gated");

        assert!(matches!(error, LoaderError::CatalogStale));
    }

    #[test]
    fn exact_build_resolver_returns_matching_fresh_build() {
        let component_id = LoaderComponentId::Fabric;
        let build_id = "fabric:1.21.5:0.16.14";

        let build = resolve_build_record_from_catalog(
            component_id,
            build_id,
            vec![build_record(component_id, build_id)],
            &catalog_state(true, false),
        )
        .expect("fresh matching catalog");

        assert_eq!(build.build_id, build_id);
    }

    fn catalog_state(fresh: bool, stale: bool) -> LoaderCatalogState {
        LoaderCatalogState {
            availability: LoaderAvailability {
                fresh,
                stale,
                cache_hit: stale,
                checked_at_ms: 1,
                last_success_at_ms: Some(1),
                last_error: None,
                last_failure_kind: None,
            },
        }
    }

    fn build_record(component_id: LoaderComponentId, build_id: &str) -> LoaderBuildRecord {
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: build_id.to_string(),
            minecraft_version: "1.21.5".to_string(),
            loader_version: "0.16.14".to_string(),
            version_id: "fabric-loader-0.16.14-1.21.5".to_string(),
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
