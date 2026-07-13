use crate::error::ContentResult;
use crate::model::{
    CanonicalContent, CanonicalId, ContentDetail, ContentKind, ContentVersion, ProjectMetadata,
    ProviderId, VersionIdentity,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// A source of installable content. One implementation per upstream service.
/// The registry fans search out across enabled providers and canonicalizes the
/// merged results.
pub trait ContentProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    fn search(
        &self,
        query: &ContentQuery,
    ) -> impl std::future::Future<Output = ContentResult<Page<CanonicalContent>>> + Send;

    fn detail(
        &self,
        id: &CanonicalId,
    ) -> impl std::future::Future<Output = ContentResult<ContentDetail>> + Send;

    fn versions(
        &self,
        id: &CanonicalId,
        filter: &LoaderGameFilter,
    ) -> impl std::future::Future<Output = ContentResult<Vec<ContentVersion>>> + Send;

    /// Resolve file hashes (sha512, lowercase hex) back to the versions that
    /// published them. Unknown hashes are simply absent from the map.
    fn identify(
        &self,
        sha512_hashes: &[String],
    ) -> impl std::future::Future<Output = ContentResult<HashMap<String, VersionIdentity>>> + Send;

    /// Resolve provider-local version IDs to their owning projects in a batch.
    /// Dependency records may omit project_id when an exact version_id is
    /// present, so resolution must canonicalize those records server-side.
    fn version_identities(
        &self,
        version_ids: &[String],
    ) -> impl std::future::Future<Output = ContentResult<HashMap<String, VersionIdentity>>> + Send;

    /// Trusted project type and title metadata, fetched in a batch so
    /// resolution never needs to rely on client-authored content kinds.
    fn metadata(
        &self,
        ids: &[CanonicalId],
    ) -> impl std::future::Future<Output = ContentResult<HashMap<CanonicalId, ProjectMetadata>>> + Send;
}
