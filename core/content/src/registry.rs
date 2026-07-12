use crate::error::{ContentError, ContentResult};
use crate::model::{
    CanonicalContent, CanonicalId, ContentDetail, ContentVersion, ProviderId, VersionIdentity,
};
use crate::modrinth::ModrinthProvider;
use crate::provider::{ContentProvider, ContentQuery, LoaderGameFilter, Page};
use std::collections::HashMap;

/// Fans requests out across the enabled providers and canonicalizes the merged
/// results. Only Modrinth is wired today; the merge seam is where a second
/// provider's records would be deduped in.
#[derive(Debug, Clone)]
pub struct ContentRegistry {
    client: reqwest::Client,
    modrinth: ModrinthProvider,
}

impl ContentRegistry {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            modrinth: ModrinthProvider::new(client.clone()),
            client,
        }
    }

    pub fn with_modrinth(client: reqwest::Client, modrinth: ModrinthProvider) -> Self {
        Self { client, modrinth }
    }

    /// The shared HTTP client, for driving verified downloads through the same
    /// connection pool the providers use.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub async fn search(&self, query: &ContentQuery) -> ContentResult<Page<CanonicalContent>> {
        let mut page = self.modrinth.search(query).await?;
        page.items = canonicalize(page.items);
        Ok(page)
    }

    pub async fn detail(&self, id: &CanonicalId) -> ContentResult<ContentDetail> {
        self.provider_for(id)?.detail(id).await
    }

    pub async fn versions(
        &self,
        id: &CanonicalId,
        filter: &LoaderGameFilter,
    ) -> ContentResult<Vec<ContentVersion>> {
        self.provider_for(id)?.versions(id, filter).await
    }

    pub async fn identify(
        &self,
        sha512_hashes: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        self.modrinth.identify(sha512_hashes).await
    }

    /// Project titles for a batch of ids, in one round trip.
    pub async fn titles(&self, ids: &[CanonicalId]) -> ContentResult<HashMap<CanonicalId, String>> {
        self.modrinth.titles(ids).await
    }

    fn provider_for(&self, id: &CanonicalId) -> ContentResult<&ModrinthProvider> {
        match provider_of(id) {
            Some(ProviderId::Modrinth) => Ok(&self.modrinth),
            None => Err(ContentError::Invalid(format!(
                "unknown provider for {}",
                id.as_str()
            ))),
        }
    }
}

fn provider_of(id: &CanonicalId) -> Option<ProviderId> {
    match id.as_str().split_once(':') {
        Some(("modrinth", _)) => Some(ProviderId::Modrinth),
        _ => None,
    }
}

/// Collapse duplicate canonical records, merging their provider sources. With one
/// provider this only guards against a project appearing twice in a page; it is
/// the hook where cross-provider records would fold together.
fn canonicalize(items: Vec<CanonicalContent>) -> Vec<CanonicalContent> {
    let mut order: Vec<CanonicalId> = Vec::with_capacity(items.len());
    let mut merged: HashMap<CanonicalId, CanonicalContent> = HashMap::with_capacity(items.len());
    for item in items {
        match merged.get_mut(&item.canonical_id) {
            Some(existing) => {
                for source in item.sources {
                    if !existing.sources.contains(&source) {
                        existing.sources.push(source);
                    }
                }
            }
            None => {
                order.push(item.canonical_id.clone());
                merged.insert(item.canonical_id.clone(), item);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| merged.remove(&id))
        .collect()
}
