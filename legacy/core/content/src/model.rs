use axial_minecraft::portable_path::{
    PortableFileName, PortablePathError, PortablePathKey, managed_content_name_is_reserved,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Exact launcher-managed content filename admitted for every supported host
/// filesystem. Provider-facing `FileRef` remains raw; accepted plans and
/// persisted ownership records carry this invariant instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ManagedContentFileName {
    enabled: PortableFileName,
    disabled: PortableFileName,
}

impl ManagedContentFileName {
    pub fn new_exact(value: &str) -> Result<Self, PortablePathError> {
        let filename = PortableFileName::new_exact(value)?;
        if filename.key().as_str().ends_with(".disabled")
            || managed_content_name_is_reserved(&filename)
        {
            return Err(PortablePathError::Unsafe);
        }
        let disabled = filename.with_suffix(".disabled")?;
        Ok(Self {
            enabled: filename,
            disabled,
        })
    }

    pub fn as_str(&self) -> &str {
        self.enabled.as_str()
    }

    pub fn key(&self) -> PortablePathKey {
        self.enabled.key()
    }

    pub fn disabled(&self) -> &PortableFileName {
        &self.disabled
    }
}

impl std::fmt::Display for ManagedContentFileName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ManagedContentFileName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedContentFileName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new_exact(&value).map_err(|_| de::Error::custom("invalid managed content filename"))
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Relevance,
    Downloads,
    Follows,
    Newest,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentQuery {
    pub kind: ContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sort: SortOrder,
    #[serde(default)]
    pub offset: u32,
    pub limit: u32,
}

impl ContentQuery {
    pub fn new(kind: ContentKind) -> Self {
        Self {
            kind,
            search: None,
            loader: None,
            game_version: None,
            categories: Vec::new(),
            sort: SortOrder::default(),
            offset: 0,
            limit: 40,
        }
    }
}

/// Narrows a project's versions to those compatible with a target instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoaderGameFilter {
    pub loader: Option<String>,
    pub game_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub offset: u32,
    pub limit: u32,
    pub total: u64,
}

/// Stable identity for a piece of content across the app, namespaced by its
/// authoritative service (`modrinth:AABBCC`).
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

impl ContentDependency {
    /// Whether this record requires the given project. Version-only provider
    /// records can identify it through the version currently installed for
    /// that project.
    pub fn requires_project(&self, project_id: &str, current_version_id: &str) -> bool {
        if self.kind != DependencyKind::Required {
            return false;
        }
        match self.project_id.as_deref() {
            Some(required_project) => required_project == project_id,
            None => self.version_id.as_deref() == Some(current_version_id),
        }
    }

    /// Whether replacing the current project with `candidate_version_id`
    /// would violate this dependency's exact version requirement.
    pub fn rejects_required_version(
        &self,
        project_id: &str,
        current_version_id: &str,
        candidate_version_id: &str,
    ) -> bool {
        self.requires_project(project_id, current_version_id)
            && self
                .version_id
                .as_deref()
                .is_some_and(|required| required != candidate_version_id)
    }
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
    /// Return the sole install authority for this version. Multiple primaries,
    /// or multiple files without a primary, are ambiguous and fail closed.
    pub fn primary_file(&self) -> Option<&FileRef> {
        let mut primaries = self.files.iter().filter(|file| file.primary);
        let first_primary = primaries.next();
        match (first_primary, primaries.next(), self.files.as_slice()) {
            (Some(file), None, _) => Some(file),
            (None, None, [file]) => Some(file),
            _ => None,
        }
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

/// Resolves a file hash back to the project and version that published it.
/// Modpack import uses the archive path to supply the content kind because the
/// provider hash lookup does not report one.
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

#[cfg(test)]
mod tests {
    use super::ManagedContentFileName;

    #[test]
    fn managed_filename_admits_both_forms_at_the_portable_byte_bound() {
        let enabled = "a".repeat(246);
        let filename = ManagedContentFileName::new_exact(&enabled).expect("bounded filename");

        assert_eq!(filename.disabled().as_str().len(), 255);
        assert!(ManagedContentFileName::new_exact(&"a".repeat(247)).is_err());
    }

    #[test]
    fn managed_filename_admits_multibyte_forms_only_when_disabled_is_portable() {
        let enabled = "é".repeat(123);
        let filename = ManagedContentFileName::new_exact(&enabled).expect("bounded filename");

        assert_eq!(filename.disabled().as_str().len(), 255);
        assert_eq!(filename.disabled().as_str().encode_utf16().count(), 132);
        assert!(ManagedContentFileName::new_exact(&"é".repeat(124)).is_err());
    }
}
