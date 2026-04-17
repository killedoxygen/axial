use crate::lifecycle::LifecycleMeta;
use crate::loaders::types::{LoaderBuildMetadata, LoaderComponentId};
use crate::version_meta::MinecraftVersionMeta;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSummary {
    pub id: String,
    pub launchable: bool,
    #[serde(default)]
    pub java_version: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VersionSubjectKind {
    #[default]
    InstalledVersion,
    MinecraftVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionLoaderAttachment {
    pub component_id: LoaderComponentId,
    pub component_name: String,
    pub build_id: String,
    pub loader_version: String,
    #[serde(default)]
    pub build_meta: LoaderBuildMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionEntry {
    #[serde(default)]
    pub subject_kind: VersionSubjectKind,
    pub id: String,
    #[serde(default)]
    pub raw_kind: String,
    #[serde(default)]
    pub release_time: String,
    #[serde(default)]
    pub minecraft_meta: MinecraftVersionMeta,
    #[serde(default)]
    pub lifecycle: LifecycleMeta,
    #[serde(default)]
    pub inherits_from: String,
    pub launchable: bool,
    pub installed: bool,
    pub status: String,
    #[serde(default)]
    pub status_detail: String,
    #[serde(default)]
    pub needs_install: String,
    #[serde(default)]
    pub java_component: String,
    #[serde(default)]
    pub java_major: i32,
    #[serde(default)]
    pub manifest_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<VersionLoaderAttachment>,
}
