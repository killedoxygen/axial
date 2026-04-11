use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSummary {
    pub id: String,
    pub launchable: bool,
    #[serde(default)]
    pub java_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub release_time: String,
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
}
