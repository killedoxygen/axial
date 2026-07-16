mod dto;

use crate::error::{ContentError, ContentResult};
use crate::model::{
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentKind, ContentVersion,
    DependencyKind, FileRef, GalleryImage, ProjectMetadata, ProviderId, ProviderRef,
    ReleaseChannel, VersionIdentity,
};
use crate::provider::{ContentProvider, ContentQuery, LoaderGameFilter, Page, SortOrder};
use std::collections::HashMap;

const DEFAULT_BASE_URL: &str = "https://api.modrinth.com/v2";
const MAX_BULK_IDS: usize = 100;
const USER_AGENT: &str = concat!(
    "mateoltd/axial/",
    env!("CARGO_PKG_VERSION"),
    " (github.com/mateoltd/axial)"
);

#[derive(Debug, Clone)]
pub struct ModrinthProvider {
    client: reqwest::Client,
    base_url: String,
}

impl ModrinthProvider {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> ContentResult<T> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(query)
            .send()
            .await?;
        parse_response(response, url).await
    }
}

impl ContentProvider for ModrinthProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Modrinth
    }

    async fn search(&self, query: &ContentQuery) -> ContentResult<Page<CanonicalContent>> {
        let facets = build_facets(query);
        let mut params: Vec<(&str, String)> = vec![
            ("index", sort_index(query.sort).to_string()),
            ("offset", query.offset.to_string()),
            ("limit", query.limit.clamp(1, 100).to_string()),
            ("facets", facets),
        ];
        if let Some(search) = query.search.as_ref().filter(|value| !value.is_empty()) {
            params.push(("query", search.clone()));
        }

        let response: dto::SearchResponse =
            self.get_json(&self.endpoint("/search"), &params).await?;
        let items = response
            .hits
            .into_iter()
            .filter_map(map_search_hit)
            .collect();
        Ok(Page {
            items,
            offset: response.offset,
            limit: response.limit,
            total: response.total_hits,
        })
    }

    async fn detail(&self, id: &CanonicalId) -> ContentResult<ContentDetail> {
        let project_id = project_id_of(id)?;
        let project: dto::Project = self
            .get_json(&self.endpoint(&format!("/project/{project_id}")), &[])
            .await?;
        let versions: Vec<dto::Version> = self
            .get_json(
                &self.endpoint(&format!("/project/{project_id}/version")),
                &[],
            )
            .await?;
        Ok(map_project_detail(project, versions))
    }

    async fn versions(
        &self,
        id: &CanonicalId,
        filter: &LoaderGameFilter,
    ) -> ContentResult<Vec<ContentVersion>> {
        let project_id = project_id_of(id)?;
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(loader) = filter.loader.as_ref().filter(|value| !value.is_empty()) {
            params.push(("loaders", json_string_array(std::slice::from_ref(loader))));
        }
        if let Some(game_version) = filter
            .game_version
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            params.push((
                "game_versions",
                json_string_array(std::slice::from_ref(game_version)),
            ));
        }
        let versions: Vec<dto::Version> = self
            .get_json(
                &self.endpoint(&format!("/project/{project_id}/version")),
                &params,
            )
            .await?;
        Ok(versions.into_iter().map(map_version).collect())
    }

    async fn metadata(
        &self,
        ids: &[CanonicalId],
    ) -> ContentResult<HashMap<CanonicalId, ProjectMetadata>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let project_ids: Vec<String> = ids.iter().filter_map(|id| project_id_of(id).ok()).collect();
        if project_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut metadata = HashMap::new();
        for chunk in project_ids.chunks(MAX_BULK_IDS) {
            let projects: Vec<dto::Project> = self
                .get_json(
                    &self.endpoint("/projects"),
                    &[("ids", json_string_array(chunk))],
                )
                .await?;
            metadata.extend(projects.into_iter().filter_map(|project| {
                let kind = kind_from_project_type(&project.project_type)?;
                Some((
                    CanonicalId::for_project(ProviderId::Modrinth, &project.id),
                    ProjectMetadata {
                        kind,
                        title: project.title,
                    },
                ))
            }));
        }
        Ok(metadata)
    }

    async fn identify(
        &self,
        sha512_hashes: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        if sha512_hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let url = self.endpoint("/version_files");
        let mut identities = HashMap::new();
        for chunk in sha512_hashes.chunks(MAX_BULK_IDS) {
            let body = serde_json::json!({
                "hashes": chunk,
                "algorithm": "sha512",
            });
            let response = self
                .client
                .post(&url)
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT, "application/json")
                .json(&body)
                .send()
                .await?;
            let resolved: HashMap<String, dto::Version> = parse_response(response, &url).await?;
            identities.extend(resolved.into_iter().filter_map(|(hash, version)| {
                map_identity(version).map(|identity| (hash, identity))
            }));
        }
        Ok(identities)
    }

    async fn version_identities(
        &self,
        version_ids: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        let mut identities = HashMap::new();
        for chunk in version_ids.chunks(MAX_BULK_IDS) {
            let versions: Vec<dto::Version> = self
                .get_json(
                    &self.endpoint("/versions"),
                    &[("ids", json_string_array(chunk))],
                )
                .await?;
            identities.extend(versions.into_iter().filter_map(|version| {
                let identity = map_identity(version)?;
                Some((identity.version_id.clone(), identity))
            }));
        }
        Ok(identities)
    }
}

async fn parse_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    context: &str,
) -> ContentResult<T> {
    let status = response.status();
    if !status.is_success() {
        return Err(ContentError::Status {
            status,
            context: context.to_string(),
        });
    }
    let bytes = response.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn kind_from_project_type(project_type: &str) -> Option<ContentKind> {
    match project_type {
        "mod" => Some(ContentKind::Mod),
        "modpack" => Some(ContentKind::Modpack),
        "resourcepack" => Some(ContentKind::ResourcePack),
        "shader" => Some(ContentKind::ShaderPack),
        _ => None,
    }
}

fn project_type_facet(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Mod => "mod",
        ContentKind::Modpack => "modpack",
        ContentKind::ResourcePack => "resourcepack",
        ContentKind::ShaderPack => "shader",
    }
}

fn sort_index(sort: SortOrder) -> &'static str {
    match sort {
        SortOrder::Relevance => "relevance",
        SortOrder::Downloads => "downloads",
        SortOrder::Follows => "follows",
        SortOrder::Newest => "newest",
        SortOrder::Updated => "updated",
    }
}

fn build_facets(query: &ContentQuery) -> String {
    let mut groups: Vec<Vec<String>> = vec![vec![format!(
        "project_type:{}",
        project_type_facet(query.kind)
    )]];
    if query.kind.filters_by_loader()
        && let Some(loader) = query.loader.as_ref().filter(|value| !value.is_empty())
    {
        groups.push(vec![format!("categories:{loader}")]);
    }
    if let Some(game_version) = query
        .game_version
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        groups.push(vec![format!("versions:{game_version}")]);
    }
    for category in &query.categories {
        if !category.is_empty() {
            groups.push(vec![format!("categories:{category}")]);
        }
    }
    serde_json::to_string(&groups).unwrap_or_else(|_| "[]".to_string())
}

fn json_string_array(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

fn project_id_of(id: &CanonicalId) -> ContentResult<String> {
    let raw = id.as_str();
    let project = raw
        .strip_prefix("modrinth:")
        .filter(|rest| !rest.is_empty())
        .ok_or_else(|| ContentError::Invalid(format!("not a modrinth id: {raw}")))?;
    Ok(project.to_string())
}

fn map_search_hit(hit: dto::SearchHit) -> Option<CanonicalContent> {
    let kind = kind_from_project_type(&hit.project_type)?;
    let categories = if hit.display_categories.is_empty() {
        hit.categories
    } else {
        hit.display_categories
    };
    Some(CanonicalContent {
        canonical_id: CanonicalId::for_project(ProviderId::Modrinth, &hit.project_id),
        kind,
        provider: ProviderId::Modrinth,
        project_id: hit.project_id.clone(),
        slug: hit.slug.clone(),
        title: hit.title,
        author: hit.author,
        summary: hit.description,
        icon_url: hit.icon_url.filter(|url| !url.is_empty()),
        downloads: hit.downloads,
        follows: hit.follows,
        categories,
        game_versions: hit.versions,
        loaders: Vec::new(),
        updated: hit.date_modified,
        sources: vec![ProviderRef {
            provider: ProviderId::Modrinth,
            project_id: hit.project_id,
            slug: hit.slug,
        }],
    })
}

fn map_project_detail(project: dto::Project, versions: Vec<dto::Version>) -> ContentDetail {
    let kind = kind_from_project_type(&project.project_type).unwrap_or(ContentKind::Mod);
    let mut categories = project.categories;
    categories.extend(project.additional_categories);
    let content = CanonicalContent {
        canonical_id: CanonicalId::for_project(ProviderId::Modrinth, &project.id),
        kind,
        provider: ProviderId::Modrinth,
        project_id: project.id.clone(),
        slug: project.slug.clone(),
        title: project.title,
        author: String::new(),
        summary: project.description,
        icon_url: project.icon_url.filter(|url| !url.is_empty()),
        downloads: project.downloads,
        follows: project.followers,
        categories,
        game_versions: project.game_versions,
        loaders: project.loaders,
        updated: project.updated,
        sources: vec![ProviderRef {
            provider: ProviderId::Modrinth,
            project_id: project.id,
            slug: project.slug,
        }],
    };
    ContentDetail {
        content,
        body: project.body,
        gallery: project
            .gallery
            .into_iter()
            .map(|entry| GalleryImage {
                url: entry.url,
                title: entry.title,
            })
            .collect(),
        versions: versions.into_iter().map(map_version).collect(),
    }
}

fn map_version(version: dto::Version) -> ContentVersion {
    ContentVersion {
        id: version.id,
        name: version.name,
        version_number: version.version_number,
        game_versions: version.game_versions,
        loaders: version.loaders,
        channel: release_channel(&version.version_type),
        published: version.date_published,
        downloads: version.downloads,
        files: version.files.into_iter().map(map_file).collect(),
        dependencies: version
            .dependencies
            .into_iter()
            .filter_map(map_dependency)
            .collect(),
    }
}

fn map_file(file: dto::VersionFile) -> FileRef {
    FileRef {
        url: file.url,
        filename: file.filename,
        sha1: file.hashes.sha1,
        sha512: file.hashes.sha512,
        size: file.size,
        primary: file.primary,
    }
}

fn map_dependency(dependency: dto::Dependency) -> Option<ContentDependency> {
    let kind = match dependency.dependency_type.as_str() {
        "required" => DependencyKind::Required,
        "optional" => DependencyKind::Optional,
        "incompatible" => DependencyKind::Incompatible,
        "embedded" => DependencyKind::Embedded,
        _ => return None,
    };
    if dependency.project_id.is_none() && dependency.version_id.is_none() {
        return None;
    }
    Some(ContentDependency {
        project_id: dependency.project_id,
        version_id: dependency.version_id,
        kind,
    })
}

fn map_identity(version: dto::Version) -> Option<VersionIdentity> {
    let game_versions = version.game_versions;
    let loaders = version.loaders;
    let dependencies = version
        .dependencies
        .into_iter()
        .filter_map(map_dependency)
        .collect();
    Some(VersionIdentity {
        provider: ProviderId::Modrinth,
        project_id: version.project_id,
        version_id: version.id,
        game_versions,
        loaders,
        dependencies,
        title: Some(version.name),
    })
}

fn release_channel(version_type: &str) -> ReleaseChannel {
    match version_type {
        "beta" => ReleaseChannel::Beta,
        "alpha" => ReleaseChannel::Alpha,
        _ => ReleaseChannel::Release,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_identity_preserves_compatibility_and_dependencies() {
        let version: dto::Version = serde_json::from_value(serde_json::json!({
            "id": "version-a",
            "project_id": "project-a",
            "name": "Project A",
            "version_number": "1.0.0",
            "game_versions": ["1.21.6"],
            "loaders": ["fabric"],
            "dependencies": [
                {
                    "project_id": "project-b",
                    "dependency_type": "incompatible"
                },
                {
                    "version_id": "version-c",
                    "dependency_type": "required"
                }
            ]
        }))
        .expect("version payload");

        let identity = map_identity(version).expect("identity");

        assert_eq!(identity.game_versions, ["1.21.6"]);
        assert_eq!(identity.loaders, ["fabric"]);
        assert_eq!(identity.dependencies.len(), 2);
        assert_eq!(
            identity.dependencies[0].project_id.as_deref(),
            Some("project-b")
        );
        assert_eq!(identity.dependencies[0].kind, DependencyKind::Incompatible);
        assert_eq!(identity.dependencies[1].project_id, None);
        assert_eq!(
            identity.dependencies[1].version_id.as_deref(),
            Some("version-c")
        );
        assert_eq!(identity.dependencies[1].kind, DependencyKind::Required);
    }
}
