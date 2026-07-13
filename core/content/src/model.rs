use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Mod,
    Modpack,
    ResourcePack,
    ShaderPack,
}

impl ContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::Modpack => "modpack",
            Self::ResourcePack => "resource_pack",
            Self::ShaderPack => "shader_pack",
        }
    }

    /// Instance-relative directory this kind installs a single file into. A
    /// modpack has none: it is a whole instance, imported rather than dropped in
    /// a folder.
    pub fn install_subdir(self) -> Option<&'static str> {
        match self {
            Self::Mod => Some("mods"),
            Self::ResourcePack => Some("resourcepacks"),
            Self::ShaderPack => Some("shaderpacks"),
            Self::Modpack => None,
        }
    }

    /// Whether upstream tags this kind with the instance's mod loader. Modrinth
    /// tags resource packs as `minecraft` and shaders as `iris`/`optifine`, so
    /// filtering those by the instance loader would match nothing.
    pub fn filters_by_loader(self) -> bool {
        matches!(self, Self::Mod | Self::Modpack)
    }

    /// Whether the target instance must have a mod loader to accept this kind.
    /// Resource packs and shaders drop into a vanilla instance fine.
    pub fn requires_mod_loader(self) -> bool {
        matches!(self, Self::Mod)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Modrinth,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modrinth => "modrinth",
        }
    }
}

/// Stable identity for a piece of content across the app. Currently namespaced by
/// its primary provider (`modrinth:AABBCC`); a future canonical resolver may merge
/// several provider projects under one id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalId(pub String);

impl CanonicalId {
    pub fn for_project(provider: ProviderId, project_id: &str) -> Self {
        Self(format!("{}:{}", provider.as_str(), project_id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The provider-local project id (the part after the provider prefix).
    pub fn project_id(&self) -> &str {
        self.0.split_once(':').map(|(_, id)| id).unwrap_or(&self.0)
    }
}

/// One provider's view of a project. Multiple refs on the same canonical record
/// mean the same content is offered by more than one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRef {
    pub provider: ProviderId,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

/// A downloadable file with the integrity facts needed to verify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub url: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha512: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Required,
    Optional,
    Incompatible,
    Embedded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDependency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    pub kind: DependencyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseChannel {
    Release,
    Beta,
    Alpha,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentVersion {
    pub id: String,
    pub name: String,
    pub version_number: String,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    pub channel: ReleaseChannel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub files: Vec<FileRef>,
    #[serde(default)]
    pub dependencies: Vec<ContentDependency>,
}

impl ContentVersion {
    pub fn primary_file(&self) -> Option<&FileRef> {
        self.files
            .iter()
            .find(|file| file.primary)
            .or_else(|| self.files.first())
    }
}

/// Search/listing summary of a canonical piece of content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalContent {
    pub canonical_id: CanonicalId,
    pub kind: ContentKind,
    pub provider: ProviderId,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub follows: u64,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(default)]
    pub sources: Vec<ProviderRef>,
}

/// Provider-authored identity used by trusted workflows that only need a
/// project's stable type and display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMetadata {
    pub kind: ContentKind,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryImage {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDetail {
    #[serde(flatten)]
    pub content: CanonicalContent,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub gallery: Vec<GalleryImage>,
    #[serde(default)]
    pub versions: Vec<ContentVersion>,
}

/// Resolves a file hash back to the project and version that published it. Powers
/// dedupe and retrofit of manually added files. The kind is not part of it: the
/// hash lookup does not report one, and the directory the file was found in is a
/// more reliable answer anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionIdentity {
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<ContentDependency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}
