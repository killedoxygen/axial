use super::types::{LoaderBuildMetadata, LoaderBuildRecord, LoaderComponentId};
use serde::{Deserialize, Serialize};

pub(crate) const INSTALLED_LOADER_METADATA_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledLoaderMetadata {
    pub(crate) schema_version: u32,
    pub(crate) component_id: LoaderComponentId,
    pub(crate) component_name: String,
    pub(crate) build_id: String,
    pub(crate) minecraft_version: String,
    pub(crate) loader_version: String,
    pub(crate) build_meta: LoaderBuildMetadata,
}

impl From<&LoaderBuildRecord> for InstalledLoaderMetadata {
    fn from(record: &LoaderBuildRecord) -> Self {
        Self {
            schema_version: INSTALLED_LOADER_METADATA_SCHEMA_VERSION,
            component_id: record.component_id,
            component_name: record.component_name.clone(),
            build_id: record.build_id.clone(),
            minecraft_version: record.minecraft_version.clone(),
            loader_version: record.loader_version.clone(),
            build_meta: record.build_meta.clone(),
        }
    }
}

pub(crate) fn installed_loader_metadata_bytes(
    record: &LoaderBuildRecord,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec_pretty(&InstalledLoaderMetadata::from(record))
}
