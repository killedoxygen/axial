use crate::download::DownloadError;
use crate::types::VersionSubjectKind;
use crate::version_meta::MinecraftVersionMeta;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

pub type LoaderBuildId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LoaderComponentId {
    #[serde(rename = "net.fabricmc.fabric-loader")]
    Fabric,
    #[serde(rename = "org.quiltmc.quilt-loader")]
    Quilt,
    #[serde(rename = "net.minecraftforge")]
    Forge,
    #[serde(rename = "net.neoforged")]
    NeoForge,
}

impl LoaderComponentId {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fabric" | "net.fabricmc.fabric-loader" => Some(Self::Fabric),
            "quilt" | "org.quiltmc.quilt-loader" => Some(Self::Quilt),
            "forge" | "net.minecraftforge" => Some(Self::Forge),
            "neoforge" | "net.neoforged" => Some(Self::NeoForge),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fabric => "net.fabricmc.fabric-loader",
            Self::Quilt => "org.quiltmc.quilt-loader",
            Self::Forge => "net.minecraftforge",
            Self::NeoForge => "net.neoforged",
        }
    }

    pub fn short_key(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderComponentRecord {
    pub id: LoaderComponentId,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderTerm {
    Recommended,
    Latest,
    Snapshot,
    PreRelease,
    ReleaseCandidate,
    Beta,
    Alpha,
    Nightly,
    Dev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoaderTermSource {
    ExplicitVersionLabel,
    ExplicitApiFlag,
    PromotionMarker,
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LoaderTermEvidence {
    pub term: LoaderTerm,
    #[serde(default)]
    pub source: LoaderTermSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoaderSelectionReason {
    Recommended,
    LatestStable,
    Stable,
    Latest,
    Unlabeled,
    LatestUnstable,
    Unstable,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoaderSelectionSource {
    ExplicitVersionLabel,
    ExplicitApiFlag,
    PromotionMarker,
    AbsenceOfRecommended,
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoaderSelectionMeta {
    #[serde(default)]
    pub default_rank: i32,
    #[serde(default)]
    pub reason: LoaderSelectionReason,
    #[serde(default)]
    pub source: LoaderSelectionSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoaderBuildMetadata {
    #[serde(default)]
    pub terms: Vec<LoaderTerm>,
    #[serde(default)]
    pub evidence: Vec<LoaderTermEvidence>,
    #[serde(default)]
    pub selection: LoaderSelectionMeta,
    #[serde(default)]
    pub display_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderGameVersion {
    #[serde(default = "minecraft_version_subject_kind")]
    pub subject_kind: VersionSubjectKind,
    pub id: String,
    #[serde(default)]
    pub release_time: String,
    #[serde(default)]
    pub minecraft_meta: MinecraftVersionMeta,
    #[serde(default)]
    pub lifecycle: crate::lifecycle::LifecycleMeta,
    #[serde(skip)]
    pub stable_hint: Option<bool>,
}

fn minecraft_version_subject_kind() -> VersionSubjectKind {
    VersionSubjectKind::MinecraftVersion
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoaderAvailability {
    pub fresh: bool,
    pub stale: bool,
    pub cache_hit: bool,
    pub checked_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoaderCatalogState {
    pub availability: LoaderAvailability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderInstallStrategy {
    FabricProfile,
    QuiltProfile,
    ForgeModern,
    ForgeLegacyInstaller,
    ForgeEarliestLegacy,
    NeoForgeModern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderArtifactKind {
    ProfileJson,
    InstallerJar,
    LegacyArchive,
    MavenArtifact,
    Generated,
    Packaged,
    LegacyExternal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoaderInstallSource {
    ProfileJson { url: String },
    InstallerJar { url: String },
    LegacyArchive { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderInstallability {
    Installable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderBuildSubjectKind {
    LoaderBuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderBuildRecord {
    #[serde(default = "loader_build_subject_kind")]
    pub subject_kind: LoaderBuildSubjectKind,
    pub component_id: LoaderComponentId,
    pub component_name: String,
    pub build_id: LoaderBuildId,
    pub minecraft_version: String,
    pub loader_version: String,
    pub version_id: String,
    #[serde(default)]
    pub build_meta: LoaderBuildMetadata,
    pub strategy: LoaderInstallStrategy,
    pub artifact_kind: LoaderArtifactKind,
    pub installability: LoaderInstallability,
    pub install_source: LoaderInstallSource,
}

fn loader_build_subject_kind() -> LoaderBuildSubjectKind {
    LoaderBuildSubjectKind::LoaderBuild
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderVersionIndex {
    pub component_id: LoaderComponentId,
    pub builds: Vec<LoaderBuildRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCatalog<T> {
    #[serde(default)]
    pub schema_version: u32,
    pub fetched_at_ms: i64,
    pub value: T,
}

impl<T> CachedCatalog<T> {
    pub fn new(value: T) -> Self {
        Self {
            schema_version: 6,
            fetched_at_ms: Utc::now().timestamp_millis(),
            value,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoaderInstallPlan {
    pub record: LoaderBuildRecord,
    pub stage_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderInstallFailureKind {
    CatalogUnavailable,
    BuildNotFound,
    ArtifactMissing,
    InvalidProfile,
    ProcessorFailed,
    VerifyFailed,
    RequestFailed,
    DownloadFailed,
    IoFailed,
    ParseFailed,
    Other,
}

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("invalid minecraft version")]
    InvalidMinecraftVersion,
    #[error("invalid loader build id")]
    InvalidBuildId,
    #[error("invalid loader component id")]
    InvalidComponentId,
    #[error("Croopor library is not configured")]
    MissingLibraryDir,
    #[error("loader catalog is unavailable: {0}")]
    CatalogUnavailable(String),
    #[error("selected loader build is not available in the upstream catalog: {0}")]
    BuildNotFound(String),
    #[error("loader artifact is not available: {0}")]
    ArtifactMissing(String),
    #[error("loader profile is invalid: {0}")]
    InvalidProfile(String),
    #[error("loader install verification failed: {0}")]
    Verify(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
    #[error("parse failed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl LoaderError {
    pub fn failure_kind(&self) -> LoaderInstallFailureKind {
        match self {
            Self::CatalogUnavailable(_) => LoaderInstallFailureKind::CatalogUnavailable,
            Self::BuildNotFound(_) => LoaderInstallFailureKind::BuildNotFound,
            Self::ArtifactMissing(_) => LoaderInstallFailureKind::ArtifactMissing,
            Self::InvalidProfile(_) => LoaderInstallFailureKind::InvalidProfile,
            Self::Verify(_) => LoaderInstallFailureKind::VerifyFailed,
            Self::Request(_) => LoaderInstallFailureKind::RequestFailed,
            Self::Download(_) => LoaderInstallFailureKind::DownloadFailed,
            Self::Parse(_) => LoaderInstallFailureKind::ParseFailed,
            Self::Io(_) => LoaderInstallFailureKind::IoFailed,
            Self::InvalidMinecraftVersion
            | Self::InvalidBuildId
            | Self::InvalidComponentId
            | Self::MissingLibraryDir
            | Self::Other(_) => LoaderInstallFailureKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LoaderGameVersion;

    #[test]
    fn loader_game_version_defaults_lifecycle_when_missing() {
        let version: LoaderGameVersion = serde_json::from_str(
            r#"{
                "id": "1.20.4"
            }"#,
        )
        .expect("loader game version should deserialize");

        assert_eq!(version.id, "1.20.4");
        assert_eq!(version.lifecycle.default_rank, 0);
        assert_eq!(
            version.subject_kind,
            crate::types::VersionSubjectKind::MinecraftVersion
        );
    }
}
