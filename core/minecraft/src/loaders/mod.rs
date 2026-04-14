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

pub use api::{
    build_id_for, infer_build_from_version_id, infer_neoforge_minecraft_version,
    installed_version_id_for, loader_components, parse_build_id,
};
pub use index::{fetch_builds, fetch_components, fetch_supported_versions, resolve_build_record};
pub use types::{
    LoaderArtifactKind, LoaderAvailability, LoaderBuildId, LoaderBuildRecord, LoaderCatalogState,
    LoaderComponentId, LoaderComponentRecord, LoaderError, LoaderGameVersion,
    LoaderInstallFailureKind, LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy,
    LoaderInstallability, LoaderVersionIndex,
};

use crate::download::DownloadProgress;
use crate::paths::loader_work_dir;
use std::fs;
use std::path::Path;

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
    let result = strategies::install_build(library_dir, &plan, send).await;
    let _ = fs::remove_dir_all(&plan.stage_dir);
    result
}

pub(crate) fn validate_version_id(version_id: &str, context: &str) -> Result<(), LoaderError> {
    if version_id.trim().is_empty() {
        return Err(LoaderError::Other(format!("{context} is empty")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_version_id;

    #[test]
    fn rejects_empty_version_ids() {
        let error = validate_version_id(" \t ", "loader build version id").expect_err("error");
        assert_eq!(error.to_string(), "loader build version id is empty");
    }
}
