use super::cache::resolve_cached;
use super::normalize::normalize_build_index;
use crate::loaders::api::{loader_components, parse_build_id};
use crate::loaders::providers;
use crate::loaders::types::{
    LoaderBuildRecord, LoaderCatalogState, LoaderComponentId, LoaderComponentRecord, LoaderError,
    LoaderGameVersion,
};
use crate::paths::loader_catalog_dir;
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
    resolve_cached(
        supported_versions_cache_path(library_dir, component_id),
        SUPPORTED_VERSIONS_TTL,
        || providers::fetch_supported_versions(component_id),
    )
    .await
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

    let (builds, _) = fetch_builds(library_dir, component_id, &minecraft_version).await?;
    builds
        .into_iter()
        .find(|build| build.build_id == build_id)
        .ok_or_else(|| {
            LoaderError::BuildNotFound(format!(
                "{} build {} for Minecraft {}",
                component_id.short_key(),
                build_id,
                minecraft_version
            ))
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
