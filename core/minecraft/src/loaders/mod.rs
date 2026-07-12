pub mod api;
pub mod artifacts;
mod compose;
mod forge_installer;
mod http;
pub mod index;
pub mod legacy;
mod processors;
pub mod providers;
pub mod strategies;
pub mod types;
pub mod workspace;

pub use api::{build_id_for, installed_version_id_for, loader_components, parse_build_id};
pub use index::{
    fetch_builds, fetch_cached_builds, fetch_components, fetch_supported_versions,
    resolve_build_record,
};
pub use types::{
    LOADER_CATALOG_SCHEMA_VERSION, LoaderArtifactKind, LoaderAvailability, LoaderBuildId,
    LoaderBuildMetadata, LoaderBuildRecord, LoaderCatalogState, LoaderComponentId,
    LoaderComponentRecord, LoaderError, LoaderGameVersion, LoaderInstallFailureKind,
    LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    LoaderProviderFailureKind, LoaderSelectionMeta, LoaderSelectionReason, LoaderSelectionSource,
    LoaderTerm, LoaderTermEvidence, LoaderTermSource, LoaderVersionIndex,
};

use crate::download::DownloadProgress;
use crate::paths::loader_work_dir;
use std::fs;
use std::path::{Component, Path};

pub async fn install_build<F>(
    library_dir: &Path,
    record: LoaderBuildRecord,
    send: F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    validate_version_id(&record.version_id, "loader build version id")?;
    let stage_dir = loader_work_dir(library_dir).join(&record.version_id);
    if stage_dir.exists() {
        let _ = fs::remove_dir_all(&stage_dir);
    }
    fs::create_dir_all(&stage_dir)?;

    let plan = LoaderInstallPlan { record, stage_dir };
    let result = Box::pin(strategies::install_build(library_dir, &plan, send)).await;
    let _ = fs::remove_dir_all(&plan.stage_dir);
    result
}

pub(crate) fn validate_version_id(version_id: &str, context: &str) -> Result<(), LoaderError> {
    validate_version_id_shape(version_id, context).map_err(LoaderError::InstallExecutionFailed)
}

pub(crate) fn validate_provider_version_id(
    version_id: &str,
    context: &str,
) -> Result<(), LoaderError> {
    validate_version_id_shape(version_id, context).map_err(LoaderError::InvalidProfile)
}

fn validate_version_id_shape(version_id: &str, context: &str) -> Result<(), String> {
    let trimmed = version_id.trim();
    if trimmed.is_empty() {
        return Err(format!("{context} is empty"));
    }
    if version_id != trimmed {
        return Err(format!("{context} contains surrounding whitespace"));
    }
    if trimmed.contains(['/', '\\']) {
        return Err(format!("{context} contains path separators"));
    }
    let mut components = Path::new(trimmed).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!("{context} is invalid"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LoaderError, validate_version_id};

    #[test]
    fn rejects_empty_version_ids() {
        let error = validate_version_id(" \t ", "loader build version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader build version id is empty"
        ));
    }

    #[test]
    fn rejects_path_traversal_version_ids() {
        for version_id in ["..", ".", "../escape", "loader/escape", "loader\\escape"] {
            let error =
                validate_version_id(version_id, "loader build version id").expect_err("error");
            assert!(
                error.to_string().contains("loader build version id"),
                "{version_id} => {}",
                error
            );
        }
    }

    #[test]
    fn rejects_whitespace_padded_version_ids() {
        let error =
            validate_version_id(" loader-id ", "loader build version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "loader build version id contains surrounding whitespace"
        ));
    }
}
