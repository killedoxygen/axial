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
    pub last_failure_kind: Option<LoaderPreOperationFailureKind>,
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

macro_rules! loader_failure_kinds {
    ($type_name:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $type_name {
            $($variant),+
        }

        impl $type_name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }
        }
    };
}

loader_failure_kinds!(LoaderPreOperationFailureKind {
    InvalidMinecraftVersion => "invalid_minecraft_version",
    InvalidBuildId => "invalid_build_id",
    CatalogUnavailable => "catalog_unavailable",
    CatalogStale => "catalog_stale",
    BuildNotFound => "build_not_found",
    ProviderHttpFailure => "provider_http_failure",
    ProviderNetworkFailure => "provider_network_failure",
    ProviderRateLimited => "provider_rate_limited",
    ProviderResponseTooLarge => "provider_response_too_large",
    ProviderSchemaInvalid => "provider_schema_invalid",
});

loader_failure_kinds!(LoaderInstallFailureKind {
    ArtifactMissing => "artifact_missing",
    InvalidProfile => "invalid_profile",
    ProviderHttpFailure => "provider_http_failure",
    ProviderNetworkFailure => "provider_network_failure",
    ProviderRateLimited => "provider_rate_limited",
    ProviderResponseTooLarge => "provider_response_too_large",
    ProviderSchemaInvalid => "provider_schema_invalid",
    ProcessorFailed => "processor_failed",
    InstallExecutionFailed => "install_execution_failed",
    VerifyFailed => "verify_failed",
    ParseFailed => "parse_failed",
});

loader_failure_kinds!(LoaderDelegatedInstallFailureKind {
    BaseInstallFailed => "base_install_failed",
    ArtifactDownloadFailed => "artifact_download_failed",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderFailureDisposition {
    PreOperation(LoaderPreOperationFailureKind),
    ActiveInstall(LoaderInstallFailureKind),
    Delegated(LoaderDelegatedInstallFailureKind),
}

impl LoaderFailureDisposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreOperation(kind) => kind.as_str(),
            Self::ActiveInstall(kind) => kind.as_str(),
            Self::Delegated(kind) => kind.as_str(),
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
    #[error("loader processor failed: {0}")]
    ProcessorFailed(String),
    #[error("loader install execution failed: {0}")]
    InstallExecutionFailed(String),
    #[error("base Minecraft install failed: {error}")]
    BaseInstallFailed {
        error: Box<DownloadError>,
        facts: Vec<ExecutionDownloadFact>,
        descriptors: Vec<SelectedDownloadArtifactDescriptor>,
    },
    #[error("loader artifact download failed")]
    ArtifactDownloadFailed {
        facts: Vec<ExecutionDownloadFact>,
        descriptors: Vec<SelectedDownloadArtifactDescriptor>,
    },
    #[error("parse failed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
}

impl LoaderError {
    pub fn failure_disposition(&self) -> LoaderFailureDisposition {
        use LoaderFailureDisposition::{ActiveInstall, Delegated, PreOperation};

        match self {
            Self::CatalogUnavailable {
                provider_failure_kind: Some(kind),
                ..
            } => PreOperation(provider_pre_operation_failure_kind(*kind)),
            Self::CatalogUnavailable { .. } => {
                PreOperation(LoaderPreOperationFailureKind::CatalogUnavailable)
            }
            Self::CatalogStale => PreOperation(LoaderPreOperationFailureKind::CatalogStale),
            Self::BuildNotFound(_) => PreOperation(LoaderPreOperationFailureKind::BuildNotFound),
            Self::ArtifactMissing(_) => ActiveInstall(LoaderInstallFailureKind::ArtifactMissing),
            Self::InvalidProfile(_) => ActiveInstall(LoaderInstallFailureKind::InvalidProfile),
            Self::ProviderUnavailable { kind, .. } => match kind {
                LoaderProviderFailureKind::Timeout | LoaderProviderFailureKind::Network => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderNetworkFailure)
                }
                LoaderProviderFailureKind::HttpRateLimited => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderRateLimited)
                }
                LoaderProviderFailureKind::HttpServer
                | LoaderProviderFailureKind::HttpStatus
                | LoaderProviderFailureKind::HttpNotFound => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderHttpFailure)
                }
                LoaderProviderFailureKind::ResponseTooLarge => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderResponseTooLarge)
                }
                LoaderProviderFailureKind::SchemaInvalid => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderSchemaInvalid)
                }
            },
            Self::ProviderDataInvalid { kind, .. } => match kind {
                LoaderProviderFailureKind::ResponseTooLarge => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderResponseTooLarge)
                }
                LoaderProviderFailureKind::SchemaInvalid => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderSchemaInvalid)
                }
                LoaderProviderFailureKind::HttpRateLimited => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderRateLimited)
                }
                LoaderProviderFailureKind::Timeout | LoaderProviderFailureKind::Network => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderNetworkFailure)
                }
                LoaderProviderFailureKind::HttpServer
                | LoaderProviderFailureKind::HttpStatus
                | LoaderProviderFailureKind::HttpNotFound => {
                    ActiveInstall(LoaderInstallFailureKind::ProviderHttpFailure)
                }
            },
            Self::Verify(_) => ActiveInstall(LoaderInstallFailureKind::VerifyFailed),
            Self::ProcessorFailed(_) => ActiveInstall(LoaderInstallFailureKind::ProcessorFailed),
            Self::InstallExecutionFailed(_) | Self::Io(_) => {
                ActiveInstall(LoaderInstallFailureKind::InstallExecutionFailed)
            }
            Self::BaseInstallFailed { .. } => {
                Delegated(LoaderDelegatedInstallFailureKind::BaseInstallFailed)
            }
            Self::ArtifactDownloadFailed { .. } => {
                Delegated(LoaderDelegatedInstallFailureKind::ArtifactDownloadFailed)
            }
            Self::Parse(_) => ActiveInstall(LoaderInstallFailureKind::ParseFailed),
            Self::InvalidMinecraftVersion => {
                PreOperation(LoaderPreOperationFailureKind::InvalidMinecraftVersion)
            }
            Self::InvalidBuildId => PreOperation(LoaderPreOperationFailureKind::InvalidBuildId),
        }
    }

    pub fn availability_failure_kind(&self) -> Option<LoaderPreOperationFailureKind> {
        match self.failure_disposition() {
            LoaderFailureDisposition::PreOperation(kind) => Some(kind),
            LoaderFailureDisposition::ActiveInstall(kind) => match kind {
                LoaderInstallFailureKind::ArtifactMissing => {
                    Some(LoaderPreOperationFailureKind::ProviderHttpFailure)
                }
                LoaderInstallFailureKind::ProviderHttpFailure => {
                    Some(LoaderPreOperationFailureKind::ProviderHttpFailure)
                }
                LoaderInstallFailureKind::ProviderNetworkFailure => {
                    Some(LoaderPreOperationFailureKind::ProviderNetworkFailure)
                }
                LoaderInstallFailureKind::ProviderRateLimited => {
                    Some(LoaderPreOperationFailureKind::ProviderRateLimited)
                }
                LoaderInstallFailureKind::ProviderResponseTooLarge => {
                    Some(LoaderPreOperationFailureKind::ProviderResponseTooLarge)
                }
                LoaderInstallFailureKind::ProviderSchemaInvalid => {
                    Some(LoaderPreOperationFailureKind::ProviderSchemaInvalid)
                }
                LoaderInstallFailureKind::InvalidProfile
                | LoaderInstallFailureKind::ProcessorFailed
                | LoaderInstallFailureKind::InstallExecutionFailed
                | LoaderInstallFailureKind::VerifyFailed
                | LoaderInstallFailureKind::ParseFailed => None,
            },
            LoaderFailureDisposition::Delegated(_) => None,
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
        self.failure_disposition().as_str()
    }
}

fn provider_pre_operation_failure_kind(
    kind: LoaderProviderFailureKind,
) -> LoaderPreOperationFailureKind {
    match kind {
        LoaderProviderFailureKind::Timeout | LoaderProviderFailureKind::Network => {
            LoaderPreOperationFailureKind::ProviderNetworkFailure
        }
        LoaderProviderFailureKind::HttpRateLimited => {
            LoaderPreOperationFailureKind::ProviderRateLimited
        }
        LoaderProviderFailureKind::HttpServer
        | LoaderProviderFailureKind::HttpStatus
        | LoaderProviderFailureKind::HttpNotFound => {
            LoaderPreOperationFailureKind::ProviderHttpFailure
        }
        LoaderProviderFailureKind::ResponseTooLarge => {
            LoaderPreOperationFailureKind::ProviderResponseTooLarge
        }
        LoaderProviderFailureKind::SchemaInvalid => {
            LoaderPreOperationFailureKind::ProviderSchemaInvalid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LoaderDelegatedInstallFailureKind, LoaderError, LoaderFailureDisposition,
        LoaderGameVersion, LoaderInstallFailureKind, LoaderPreOperationFailureKind,
    };
    use std::collections::HashSet;
    use std::io;

    #[test]
    fn active_loader_failure_inventory_is_unique_and_covers_runtime_categories() {
        let names = LoaderInstallFailureKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), LoaderInstallFailureKind::ALL.len());
        assert_eq!(
            LoaderError::ProcessorFailed("processor exit".to_string()).failure_disposition(),
            LoaderFailureDisposition::ActiveInstall(LoaderInstallFailureKind::ProcessorFailed)
        );
        assert_eq!(
            LoaderError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
                .failure_disposition(),
            LoaderFailureDisposition::ActiveInstall(
                LoaderInstallFailureKind::InstallExecutionFailed
            )
        );
        assert_eq!(
            LoaderError::Io(io::Error::other("disk failure")).failure_disposition(),
            LoaderFailureDisposition::ActiveInstall(
                LoaderInstallFailureKind::InstallExecutionFailed
            )
        );
    }

    #[test]
    fn loader_failure_inventories_are_closed_and_boundary_specific() {
        for names in [
            LoaderPreOperationFailureKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect::<HashSet<_>>(),
            LoaderInstallFailureKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect::<HashSet<_>>(),
            LoaderDelegatedInstallFailureKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect::<HashSet<_>>(),
        ] {
            assert!(!names.is_empty());
        }
        assert_eq!(LoaderPreOperationFailureKind::ALL.len(), 10);
        assert_eq!(LoaderInstallFailureKind::ALL.len(), 11);
        assert_eq!(LoaderDelegatedInstallFailureKind::ALL.len(), 2);

        assert_eq!(
            LoaderError::InvalidBuildId.failure_disposition(),
            LoaderFailureDisposition::PreOperation(LoaderPreOperationFailureKind::InvalidBuildId)
        );
        assert_eq!(
            LoaderError::ArtifactDownloadFailed {
                facts: Vec::new(),
                descriptors: Vec::new(),
            }
            .failure_disposition(),
            LoaderFailureDisposition::Delegated(
                LoaderDelegatedInstallFailureKind::ArtifactDownloadFailed
            )
        );
    }

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
