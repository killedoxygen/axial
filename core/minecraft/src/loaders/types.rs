use crate::download::{DownloadError, ExecutionDownloadFact, SelectedDownloadArtifactDescriptor};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_kind: Option<LoaderInstallFailureKind>,
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

pub const LOADER_CATALOG_SCHEMA_VERSION: u32 = 8;

impl<T> CachedCatalog<T> {
    pub fn new(value: T) -> Self {
        Self {
            schema_version: LOADER_CATALOG_SCHEMA_VERSION,
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
    CatalogStale,
    BuildNotFound,
    ArtifactMissing,
    InvalidProfile,
    ProviderHttpFailure,
    ProviderNetworkFailure,
    ProviderRateLimited,
    ProviderResponseTooLarge,
    ProviderSchemaInvalid,
    ProcessorFailed,
    VerifyFailed,
    BaseInstallFailed,
    RequestFailed,
    DownloadFailed,
    IoFailed,
    ParseFailed,
    Other,
}

impl LoaderInstallFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CatalogUnavailable => "catalog_unavailable",
            Self::CatalogStale => "catalog_stale",
            Self::BuildNotFound => "build_not_found",
            Self::ArtifactMissing => "artifact_missing",
            Self::InvalidProfile => "invalid_profile",
            Self::ProviderHttpFailure => "provider_http_failure",
            Self::ProviderNetworkFailure => "provider_network_failure",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::ProviderResponseTooLarge => "provider_response_too_large",
            Self::ProviderSchemaInvalid => "provider_schema_invalid",
            Self::ProcessorFailed => "processor_failed",
            Self::VerifyFailed => "verify_failed",
            Self::BaseInstallFailed => "base_install_failed",
            Self::RequestFailed => "request_failed",
            Self::DownloadFailed => "download_failed",
            Self::IoFailed => "io_failed",
            Self::ParseFailed => "parse_failed",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderProviderFailureKind {
    Network,
    Timeout,
    HttpNotFound,
    HttpRateLimited,
    HttpServer,
    HttpStatus,
    ResponseTooLarge,
    SchemaInvalid,
}

impl LoaderProviderFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::HttpNotFound => "http_not_found",
            Self::HttpRateLimited => "http_rate_limited",
            Self::HttpServer => "http_server",
            Self::HttpStatus => "http_status",
            Self::ResponseTooLarge => "response_too_large",
            Self::SchemaInvalid => "schema_invalid",
        }
    }
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
    #[error("loader catalog is unavailable: {message}")]
    CatalogUnavailable {
        message: String,
        provider_failure_kind: Option<LoaderProviderFailureKind>,
        provider_status: Option<u16>,
    },
    #[error("loader catalog must be refreshed before selecting this build")]
    CatalogStale,
    #[error("selected loader build is not available in the upstream catalog: {0}")]
    BuildNotFound(String),
    #[error("loader artifact is not available: {0}")]
    ArtifactMissing(String),
    #[error("loader profile is invalid: {0}")]
    InvalidProfile(String),
    #[error("loader provider is unavailable: {kind:?}")]
    ProviderUnavailable {
        kind: LoaderProviderFailureKind,
        status: Option<u16>,
    },
    #[error("loader provider data is invalid: {kind:?}")]
    ProviderDataInvalid {
        kind: LoaderProviderFailureKind,
        status: Option<u16>,
    },
    #[error("loader install verification failed: {0}")]
    Verify(String),
    #[error("base Minecraft install failed")]
    BaseInstallFailed {
        facts: Vec<ExecutionDownloadFact>,
        descriptors: Vec<SelectedDownloadArtifactDescriptor>,
    },
    #[error("loader artifact download failed")]
    ArtifactDownloadFailed {
        facts: Vec<ExecutionDownloadFact>,
        descriptors: Vec<SelectedDownloadArtifactDescriptor>,
    },
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
            Self::CatalogUnavailable {
                provider_failure_kind: Some(kind),
                ..
            } => provider_install_failure_kind(*kind),
            Self::CatalogUnavailable { .. } => LoaderInstallFailureKind::CatalogUnavailable,
            Self::CatalogStale => LoaderInstallFailureKind::CatalogStale,
            Self::BuildNotFound(_) => LoaderInstallFailureKind::BuildNotFound,
            Self::ArtifactMissing(_) => LoaderInstallFailureKind::ArtifactMissing,
            Self::InvalidProfile(_) => LoaderInstallFailureKind::InvalidProfile,
            Self::ProviderUnavailable { kind, .. } => match kind {
                LoaderProviderFailureKind::Timeout | LoaderProviderFailureKind::Network => {
                    LoaderInstallFailureKind::ProviderNetworkFailure
                }
                LoaderProviderFailureKind::HttpRateLimited => {
                    LoaderInstallFailureKind::ProviderRateLimited
                }
                LoaderProviderFailureKind::HttpServer
                | LoaderProviderFailureKind::HttpStatus
                | LoaderProviderFailureKind::HttpNotFound => {
                    LoaderInstallFailureKind::ProviderHttpFailure
                }
                LoaderProviderFailureKind::ResponseTooLarge => {
                    LoaderInstallFailureKind::ProviderResponseTooLarge
                }
                LoaderProviderFailureKind::SchemaInvalid => {
                    LoaderInstallFailureKind::ProviderSchemaInvalid
                }
            },
            Self::ProviderDataInvalid { kind, .. } => match kind {
                LoaderProviderFailureKind::ResponseTooLarge => {
                    LoaderInstallFailureKind::ProviderResponseTooLarge
                }
                LoaderProviderFailureKind::SchemaInvalid => {
                    LoaderInstallFailureKind::ProviderSchemaInvalid
                }
                LoaderProviderFailureKind::HttpRateLimited => {
                    LoaderInstallFailureKind::ProviderRateLimited
                }
                LoaderProviderFailureKind::Timeout | LoaderProviderFailureKind::Network => {
                    LoaderInstallFailureKind::ProviderNetworkFailure
                }
                LoaderProviderFailureKind::HttpServer
                | LoaderProviderFailureKind::HttpStatus
                | LoaderProviderFailureKind::HttpNotFound => {
                    LoaderInstallFailureKind::ProviderHttpFailure
                }
            },
            Self::Verify(_) => LoaderInstallFailureKind::VerifyFailed,
            Self::BaseInstallFailed { .. } => LoaderInstallFailureKind::BaseInstallFailed,
            Self::ArtifactDownloadFailed { .. } => LoaderInstallFailureKind::DownloadFailed,
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

    pub fn provider_failure_kind(&self) -> Option<LoaderProviderFailureKind> {
        match self {
            Self::CatalogUnavailable {
                provider_failure_kind,
                ..
            } => *provider_failure_kind,
            Self::ProviderUnavailable { kind, .. } | Self::ProviderDataInvalid { kind, .. } => {
                Some(*kind)
            }
            _ => None,
        }
    }

    pub fn provider_status(&self) -> Option<u16> {
        match self {
            Self::CatalogUnavailable {
                provider_status, ..
            } => *provider_status,
            Self::ProviderUnavailable { status, .. } | Self::ProviderDataInvalid { status, .. } => {
                *status
            }
            _ => None,
        }
    }

    pub fn safe_status_label(&self) -> &'static str {
        self.failure_kind().as_str()
    }
}

fn provider_install_failure_kind(kind: LoaderProviderFailureKind) -> LoaderInstallFailureKind {
    match kind {
        LoaderProviderFailureKind::Timeout | LoaderProviderFailureKind::Network => {
            LoaderInstallFailureKind::ProviderNetworkFailure
        }
        LoaderProviderFailureKind::HttpRateLimited => LoaderInstallFailureKind::ProviderRateLimited,
        LoaderProviderFailureKind::HttpServer
        | LoaderProviderFailureKind::HttpStatus
        | LoaderProviderFailureKind::HttpNotFound => LoaderInstallFailureKind::ProviderHttpFailure,
        LoaderProviderFailureKind::ResponseTooLarge => {
            LoaderInstallFailureKind::ProviderResponseTooLarge
        }
        LoaderProviderFailureKind::SchemaInvalid => LoaderInstallFailureKind::ProviderSchemaInvalid,
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

    #[test]
    fn loader_game_version_serializes_stable_hint_for_catalog_cache() {
        let version = LoaderGameVersion {
            subject_kind: crate::types::VersionSubjectKind::MinecraftVersion,
            id: "26.2".to_string(),
            release_time: String::new(),
            minecraft_meta: crate::version_meta::MinecraftVersionMeta::default(),
            lifecycle: crate::lifecycle::LifecycleMeta::default(),
            stable_hint: Some(false),
        };

        let encoded = serde_json::to_string(&version).expect("serialize loader game version");
        assert!(encoded.contains("\"stable_hint\":false"));

        let decoded: LoaderGameVersion =
            serde_json::from_str(&encoded).expect("deserialize loader game version");
        assert_eq!(decoded.stable_hint, Some(false));
    }
}
