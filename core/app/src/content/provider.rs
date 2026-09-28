//! Modrinth response decoding and validation, independent of transport.

use super::catalog::{ContentError, ContentResult};
use super::model::*;
use std::collections::HashSet;

pub(super) const MAX_PROVIDER_METADATA_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_PROVIDER_DETAIL_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_DETAIL_BODY_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_PROVIDER_BATCH_ITEMS: usize = 4096;
const MAX_PROVIDER_ID_BYTES: usize = 256;

pub(super) mod dto {
    use serde::de::{MapAccess, Visitor};
    use serde::{Deserialize, Deserializer};
    use std::fmt;

    #[derive(Debug, Deserialize)]
    pub(crate) struct SearchResponse {
        pub hits: Vec<SearchHit>,
        pub offset: u32,
        pub limit: u32,
        pub total_hits: u64,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct SearchHit {
        pub project_id: String,
        #[serde(default)]
        pub slug: Option<String>,
        pub title: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub display_categories: Vec<String>,
        #[serde(default)]
        pub categories: Vec<String>,
        #[serde(default)]
        pub downloads: u64,
        #[serde(default)]
        pub follows: u64,
        #[serde(default)]
        pub icon_url: Option<String>,
        #[serde(default)]
        pub author: String,
        #[serde(default)]
        pub versions: Vec<String>,
        #[serde(default)]
        pub date_modified: Option<String>,
        #[serde(default)]
        pub project_type: String,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Project {
        pub id: String,
        #[serde(default)]
        pub slug: Option<String>,
        pub title: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub body: String,
        #[serde(default)]
        pub categories: Vec<String>,
        #[serde(default)]
        pub additional_categories: Vec<String>,
        #[serde(default)]
        pub icon_url: Option<String>,
        #[serde(default)]
        pub downloads: u64,
        #[serde(default)]
        pub followers: u64,
        #[serde(default)]
        pub gallery: Vec<GalleryEntry>,
        #[serde(default)]
        pub game_versions: Vec<String>,
        #[serde(default)]
        pub loaders: Vec<String>,
        #[serde(default)]
        pub project_type: String,
        #[serde(default)]
        pub updated: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct GalleryEntry {
        pub url: String,
        #[serde(default)]
        pub title: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Version {
        pub id: String,
        pub project_id: String,
        pub name: String,
        pub version_number: String,
        #[serde(default)]
        pub dependencies: Vec<Dependency>,
        #[serde(default)]
        pub game_versions: Vec<String>,
        #[serde(default)]
        pub version_type: String,
        #[serde(default)]
        pub loaders: Vec<String>,
        #[serde(default)]
        pub downloads: u64,
        #[serde(default)]
        pub date_published: Option<String>,
        #[serde(default)]
        pub files: Vec<VersionFile>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Dependency {
        #[serde(default)]
        pub version_id: Option<String>,
        #[serde(default)]
        pub project_id: Option<String>,
        pub dependency_type: DependencyType,
    }

    #[derive(Debug, Clone, Copy, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum DependencyType {
        Required,
        Optional,
        Incompatible,
        Embedded,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct VersionFile {
        pub hashes: Hashes,
        pub url: String,
        pub filename: String,
        #[serde(default)]
        pub primary: bool,
        #[serde(default)]
        pub size: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Hashes {
        #[serde(default)]
        pub sha1: Option<String>,
        #[serde(default)]
        pub sha512: Option<String>,
    }

    pub(crate) struct VersionFilesResponse(pub Vec<(String, Version)>);

    impl<'de> Deserialize<'de> for VersionFilesResponse {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct VersionFilesVisitor;

            impl<'de> Visitor<'de> for VersionFilesVisitor {
                type Value = VersionFilesResponse;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a map from requested file hashes to content versions")
                }

                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                    while let Some(entry) = map.next_entry()? {
                        entries.push(entry);
                    }
                    Ok(VersionFilesResponse(entries))
                }
            }

            deserializer.deserialize_map(VersionFilesVisitor)
        }
    }
}

pub(super) fn kind_from_project_type(project_type: &str) -> Option<ContentKind> {
    match project_type {
        "mod" => Some(ContentKind::Mod),
        "modpack" => Some(ContentKind::Modpack),
        "resourcepack" => Some(ContentKind::ResourcePack),
        "shader" => Some(ContentKind::ShaderPack),
        _ => None,
    }
}

pub(super) fn project_type_facet(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Mod => "mod",
        ContentKind::Modpack => "modpack",
        ContentKind::ResourcePack => "resourcepack",
        ContentKind::ShaderPack => "shader",
    }
}

pub(super) fn sort_index(sort: SortOrder) -> &'static str {
    match sort {
        SortOrder::Relevance => "relevance",
        SortOrder::Downloads => "downloads",
        SortOrder::Follows => "follows",
        SortOrder::Newest => "newest",
        SortOrder::Updated => "updated",
    }
}

pub(super) fn build_facets(query: &ContentQuery) -> String {
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

pub(super) fn json_string_array(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

pub(super) fn validate_query(query: &ContentQuery) -> ContentResult<()> {
    if query
        .search
        .as_ref()
        .is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control))
        || query.categories.len() > 32
    {
        return Err(ContentError::Invalid(
            "content search input exceeds its bounds".to_string(),
        ));
    }
    validate_filter(&LoaderGameFilter {
        loader: query.loader.clone(),
        game_version: query.game_version.clone(),
    })?;
    for value in &query.categories {
        validate_facet(value)?;
    }
    Ok(())
}

pub(super) fn validate_filter(filter: &LoaderGameFilter) -> ContentResult<()> {
    for value in [&filter.loader, &filter.game_version].into_iter().flatten() {
        validate_facet(value)?;
    }
    Ok(())
}

fn validate_facet(value: &str) -> ContentResult<()> {
    if value.len() > 128 || value.chars().any(|ch| ch.is_control() || ch == ':') {
        return Err(ContentError::Invalid(
            "invalid content search filter".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn map_search_page(
    query: &ContentQuery,
    response: dto::SearchResponse,
) -> ContentResult<Page<CanonicalContent>> {
    if response.offset != query.offset
        || response.limit != query.limit.clamp(1, 100)
        || response.hits.len() > response.limit as usize
        || (!response.hits.is_empty()
            && response.total_hits < u64::from(response.offset) + response.hits.len() as u64)
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "content search pagination does not match the request".to_string(),
        ));
    }
    let mut seen = HashSet::with_capacity(response.hits.len());
    let items = response
        .hits
        .into_iter()
        .map(|hit| {
            let item = map_search_hit(hit)?;
            if item.kind != query.kind || !seen.insert(item.project_id.clone()) {
                return Err(ContentError::ProviderMetadataInvalid(
                    "content search returned an unexpected or duplicate project".to_string(),
                ));
            }
            Ok(item)
        })
        .collect::<ContentResult<Vec<_>>>()?;
    Ok(Page {
        items,
        offset: response.offset,
        limit: response.limit,
        total: response.total_hits,
    })
}

pub(super) fn validate_batch_item_count(count: usize) -> ContentResult<()> {
    if count > MAX_PROVIDER_BATCH_ITEMS {
        return Err(ContentError::Invalid(
            "content provider batch input exceeds its item bound".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_unique_identity_inputs(label: &str, values: &[String]) -> ContentResult<()> {
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if !valid_provider_identity(value) {
            return Err(ContentError::Invalid(format!(
                "content provider {label} input is invalid"
            )));
        }
        if !seen.insert(value.as_str()) {
            return Err(duplicate_batch_input(label));
        }
    }
    Ok(())
}

pub(super) fn validate_unique_hash_inputs(values: &[String]) -> ContentResult<()> {
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if !valid_sha512(value) {
            return Err(ContentError::Invalid(
                "content provider hash input is invalid".to_string(),
            ));
        }
        if !seen.insert(value.as_str()) {
            return Err(duplicate_batch_input("hash"));
        }
    }
    Ok(())
}

pub(super) fn duplicate_batch_input(label: &str) -> ContentError {
    ContentError::Invalid(format!(
        "content provider batch contains a duplicate {label} input"
    ))
}

pub(super) fn validate_batch_result_identity(
    label: &str,
    value: &str,
    requested: &HashSet<&str>,
    seen: &mut HashSet<String>,
) -> ContentResult<()> {
    let identity_valid = if label == "hash" {
        valid_sha512(value)
    } else {
        valid_provider_identity(value)
    };
    if !identity_valid || !requested.contains(value) {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "content provider returned an unexpected {label} identity"
        )));
    }
    if !seen.insert(value.to_string()) {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "content provider returned a duplicate {label} identity"
        )));
    }
    Ok(())
}

pub(super) fn project_id_of(id: &CanonicalId) -> ContentResult<String> {
    let raw = id.as_str();
    let project = raw
        .strip_prefix("modrinth:")
        .filter(|rest| valid_provider_identity(rest))
        .ok_or_else(|| ContentError::Invalid("invalid Modrinth project identity".to_string()))?;
    Ok(project.to_string())
}

pub(super) fn map_search_hit(hit: dto::SearchHit) -> ContentResult<CanonicalContent> {
    validate_provider_identity("project", &hit.project_id)?;
    let kind = kind_from_project_type(&hit.project_type).ok_or_else(|| {
        ContentError::ProviderMetadataInvalid("unknown content project type".to_string())
    })?;
    let categories = if hit.display_categories.is_empty() {
        hit.categories
    } else {
        hit.display_categories
    };
    Ok(CanonicalContent {
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
    })
}

pub(super) fn map_project_detail(
    requested_project_id: &str,
    project: dto::Project,
    versions: Vec<dto::Version>,
) -> ContentResult<ContentDetail> {
    if project.id != requested_project_id {
        return Err(ContentError::ProviderMetadataInvalid(
            "content provider returned detail for a different project".to_string(),
        ));
    }
    if project.body.len() > MAX_DETAIL_BODY_BYTES {
        return Err(ContentError::ProviderMetadataInvalid(
            "content detail body exceeded its size bound".to_string(),
        ));
    }
    validate_provider_identity("project", &project.id)?;
    let kind = kind_from_project_type(&project.project_type).ok_or_else(|| {
        ContentError::ProviderMetadataInvalid("unknown content project type".to_string())
    })?;
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
    };
    Ok(ContentDetail {
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
        versions: map_project_versions(requested_project_id, versions)?,
    })
}

pub(super) fn map_project_versions(
    requested_project_id: &str,
    versions: Vec<dto::Version>,
) -> ContentResult<Vec<ContentVersion>> {
    if versions.len() > MAX_PROVIDER_BATCH_ITEMS {
        return Err(ContentError::ProviderMetadataInvalid(
            "too many content versions".to_string(),
        ));
    }
    let mut seen = HashSet::with_capacity(versions.len());
    versions
        .into_iter()
        .map(|version| {
            if version.project_id != requested_project_id {
                return Err(ContentError::ProviderMetadataInvalid(
                    "content provider returned a version for a different project".to_string(),
                ));
            }
            if !seen.insert(version.id.clone()) {
                return Err(ContentError::ProviderMetadataInvalid(
                    "content provider returned a duplicate version identity".to_string(),
                ));
            }
            map_version(version)
        })
        .collect()
}

pub(super) fn map_version(version: dto::Version) -> ContentResult<ContentVersion> {
    validate_provider_identity("version", &version.id)?;
    validate_provider_identity("project", &version.project_id)?;
    if version.dependencies.len() > 256 || version.files.len() > 256 {
        return Err(ContentError::ProviderMetadataInvalid(
            "too many content version records".to_string(),
        ));
    }
    Ok(ContentVersion {
        id: version.id,
        name: version.name,
        version_number: version.version_number,
        game_versions: version.game_versions,
        loaders: version.loaders,
        channel: release_channel(&version.version_type)?,
        published: version.date_published,
        downloads: version.downloads,
        files: version
            .files
            .into_iter()
            .map(map_file)
            .collect::<ContentResult<Vec<_>>>()?,
        dependencies: version
            .dependencies
            .into_iter()
            .map(map_dependency)
            .collect::<ContentResult<Vec<_>>>()?,
    })
}

pub(super) fn map_file(file: dto::VersionFile) -> ContentResult<FileRef> {
    if file.url.len() > 8192 || file.filename.is_empty() || file.filename.len() > 1024 {
        return Err(ContentError::ProviderMetadataInvalid(
            "invalid content file reference".to_string(),
        ));
    }
    let url = reqwest::Url::parse(&file.url).map_err(|_| {
        ContentError::ProviderMetadataInvalid("invalid content file URL".to_string())
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "invalid content file URL".to_string(),
        ));
    }
    if file
        .hashes
        .sha1
        .as_deref()
        .is_some_and(|value| !valid_hash(value, 40))
        || file
            .hashes
            .sha512
            .as_deref()
            .is_some_and(|value| !valid_sha512(value))
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "invalid content file checksum".to_string(),
        ));
    }
    Ok(FileRef {
        url: file.url,
        filename: file.filename,
        sha1: file.hashes.sha1,
        sha512: file.hashes.sha512,
        size: file.size,
        primary: file.primary,
    })
}

pub(super) fn map_dependency(dependency: dto::Dependency) -> ContentResult<ContentDependency> {
    let kind = match dependency.dependency_type {
        dto::DependencyType::Required => DependencyKind::Required,
        dto::DependencyType::Optional => DependencyKind::Optional,
        dto::DependencyType::Incompatible => DependencyKind::Incompatible,
        dto::DependencyType::Embedded => DependencyKind::Embedded,
    };
    if let Some(project_id) = dependency.project_id.as_deref() {
        validate_provider_identity("dependency project", project_id)?;
    }
    if let Some(version_id) = dependency.version_id.as_deref() {
        validate_provider_identity("dependency version", version_id)?;
    }
    if dependency.project_id.is_none() && dependency.version_id.is_none() {
        return Err(ContentError::ProviderMetadataInvalid(
            "content dependency has no project or version identity".to_string(),
        ));
    }
    Ok(ContentDependency {
        project_id: dependency.project_id,
        version_id: dependency.version_id,
        kind,
    })
}

pub(super) fn map_identity(version: dto::Version) -> ContentResult<VersionIdentity> {
    validate_provider_identity("version", &version.id)?;
    validate_provider_identity("project", &version.project_id)?;
    if version.dependencies.len() > 256 {
        return Err(ContentError::ProviderMetadataInvalid(
            "too many content dependencies".to_string(),
        ));
    }
    let game_versions = version.game_versions;
    let loaders = version.loaders;
    let dependencies = version
        .dependencies
        .into_iter()
        .map(map_dependency)
        .collect::<ContentResult<Vec<_>>>()?;
    Ok(VersionIdentity {
        provider: ProviderId::Modrinth,
        project_id: version.project_id,
        version_id: version.id,
        game_versions,
        loaders,
        dependencies,
        title: Some(version.name),
    })
}

pub(super) fn release_channel(version_type: &str) -> ContentResult<ReleaseChannel> {
    match version_type {
        "release" => Ok(ReleaseChannel::Release),
        "beta" => Ok(ReleaseChannel::Beta),
        "alpha" => Ok(ReleaseChannel::Alpha),
        _ => Err(ContentError::ProviderMetadataInvalid(
            "content version has an unknown release channel".to_string(),
        )),
    }
}

pub(super) fn validate_provider_identity(label: &str, value: &str) -> ContentResult<()> {
    if !valid_provider_identity(value) {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "content {label} identity is invalid"
        )));
    }
    Ok(())
}

pub(super) fn valid_provider_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROVIDER_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) fn valid_sha512(value: &str) -> bool {
    valid_hash(value, 128)
}

fn valid_hash(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn version(project: &str, id: &str) -> dto::Version {
        serde_json::from_value(json!({
            "project_id": project, "id": id, "name": "Release", "version_number": "1.0",
            "version_type": "release", "loaders": ["fabric"], "game_versions": ["1.21.6"]
        }))
        .unwrap()
    }

    #[test]
    fn filters_preserve_all_content_kinds_and_sort_orders() {
        for (kind, upstream, loader_applies) in [
            (ContentKind::Mod, "mod", true),
            (ContentKind::Modpack, "modpack", true),
            (ContentKind::ShaderPack, "shader", false),
            (ContentKind::ResourcePack, "resourcepack", false),
        ] {
            let mut query = ContentQuery::new(kind);
            query.loader = Some("fabric".into());
            query.game_version = Some("1.21.6".into());
            query.categories = vec!["adventure".into(), "".into()];
            let facets: Vec<Vec<String>> = serde_json::from_str(&build_facets(&query)).unwrap();
            assert_eq!(facets[0], [format!("project_type:{upstream}")]);
            assert_eq!(
                facets.contains(&vec!["categories:fabric".into()]),
                loader_applies
            );
            assert!(facets.contains(&vec!["versions:1.21.6".into()]));
            assert!(facets.contains(&vec!["categories:adventure".into()]));
            assert_eq!(facets.len(), if loader_applies { 4 } else { 3 });
        }
        assert_eq!(
            [
                SortOrder::Relevance,
                SortOrder::Downloads,
                SortOrder::Follows,
                SortOrder::Newest,
                SortOrder::Updated
            ]
            .map(sort_index),
            ["relevance", "downloads", "follows", "newest", "updated"]
        );
    }

    #[test]
    fn invalid_queries_and_path_identities_fail_before_request() {
        for invalid in [
            "",
            "../project",
            "project?query",
            "a/b",
            "a%2Fb",
            "two words",
            "a\n",
        ] {
            assert!(project_id_of(&CanonicalId(format!("modrinth:{invalid}"))).is_err());
        }
        assert!(project_id_of(&CanonicalId("elsewhere:project".into())).is_err());
        assert_eq!(
            project_id_of(&CanonicalId("modrinth:project_a-1".into())).unwrap(),
            "project_a-1"
        );
        let mut query = ContentQuery::new(ContentKind::Mod);
        query.search = Some("x".repeat(513));
        assert!(validate_query(&query).is_err());
        query.search = Some("日本語".into());
        validate_query(&query).unwrap();
        query.categories = vec!["categories:override".into()];
        assert!(validate_query(&query).is_err());
    }

    #[test]
    fn search_preserves_display_categories_and_exact_pagination() {
        let mut query = ContentQuery::new(ContentKind::Mod);
        query.offset = 40;
        query.limit = 1;
        let payload = json!({"hits":[{"project_id":"p1","title":"Name","project_type":"mod",
            "categories":["fabric"],"display_categories":["optimization"],"icon_url":"",
            "author":"Author","versions":["1.21.6"],"date_modified":"2026-09-01"}],
            "offset":40,"limit":1,"total_hits":100});
        let page =
            map_search_page(&query, serde_json::from_value(payload.clone()).unwrap()).unwrap();
        assert_eq!((page.offset, page.limit, page.total), (40, 1, 100));
        assert_eq!(page.items[0].categories, ["optimization"]);
        assert_eq!(page.items[0].canonical_id.as_str(), "modrinth:p1");
        assert_eq!(page.items[0].icon_url, None);
        for (field, wrong) in [
            ("offset", json!(0)),
            ("limit", json!(2)),
            ("total_hits", json!(40)),
        ] {
            let mut malformed = payload.clone();
            malformed[field] = wrong;
            assert!(map_search_page(&query, serde_json::from_value(malformed).unwrap()).is_err());
        }
        let mut malformed = payload.clone();
        malformed["hits"][0]["project_type"] = json!("plugin");
        assert!(map_search_page(&query, serde_json::from_value(malformed).unwrap()).is_err());
        let mut malformed = payload;
        let duplicate = malformed["hits"][0].clone();
        malformed["hits"].as_array_mut().unwrap().push(duplicate);
        assert!(map_search_page(&query, serde_json::from_value(malformed).unwrap()).is_err());
    }

    #[test]
    fn detail_preserves_gallery_order_and_refuses_foreign_project_or_unknown_type() {
        let payload = json!({"id":"p1","title":"Pack","project_type":"resourcepack","body":"# Body",
            "categories":["16x"],"additional_categories":["vanilla-like"],
            "gallery":[{"url":"https://cdn.modrinth.com/first.png","title":"First"},
                {"url":"https://cdn.modrinth.com/second.png"}]});
        let detail = map_project_detail(
            "p1",
            serde_json::from_value(payload.clone()).unwrap(),
            vec![],
        )
        .unwrap();
        assert_eq!(detail.content.kind, ContentKind::ResourcePack);
        assert_eq!(detail.content.categories, ["16x", "vanilla-like"]);
        assert_eq!(detail.body, "# Body");
        assert_eq!(detail.gallery[0].title.as_deref(), Some("First"));
        assert_eq!(detail.gallery[1].title, None);
        assert!(
            map_project_detail(
                "p2",
                serde_json::from_value(payload.clone()).unwrap(),
                vec![]
            )
            .is_err()
        );
        let mut unknown = payload;
        unknown["project_type"] = json!("plugin");
        assert!(
            map_project_detail("p1", serde_json::from_value(unknown).unwrap(), vec![]).is_err()
        );
    }

    #[test]
    fn detail_and_batch_payloads_have_aggregate_bounds() {
        let project = |body: String| {
            serde_json::from_value(json!({"id":"p","title":"P","project_type":"mod","body":body}))
                .unwrap()
        };
        map_project_detail("p", project("x".repeat(MAX_DETAIL_BODY_BYTES)), vec![]).unwrap();
        assert!(
            map_project_detail("p", project("x".repeat(MAX_DETAIL_BODY_BYTES + 1)), vec![])
                .is_err()
        );
        validate_batch_item_count(MAX_PROVIDER_BATCH_ITEMS).unwrap();
        assert!(validate_batch_item_count(MAX_PROVIDER_BATCH_ITEMS + 1).is_err());
    }

    #[test]
    fn versions_refuse_foreign_projects_duplicates_and_ambiguous_release_channels() {
        assert!(map_project_versions("p1", vec![version("p2", "v1")]).is_err());
        assert!(
            map_project_versions("p1", vec![version("p1", "v1"), version("p1", "v1")]).is_err()
        );
        let mut bad = version("p1", "v1");
        bad.version_type = "candidate".into();
        assert!(map_version(bad).is_err());
        let valid = map_version(version("p1", "v1")).unwrap();
        assert_eq!(valid.game_versions, ["1.21.6"]);
        assert_eq!(valid.loaders, ["fabric"]);
    }

    #[test]
    fn version_dependencies_preserve_pins_and_incompatibilities_and_reject_unknown_kinds() {
        let mut raw = version("p1", "v1");
        raw.dependencies = serde_json::from_value(json!([
            {"version_id":"pinned","dependency_type":"required"},
            {"project_id":"p2","dependency_type":"incompatible"},
            {"project_id":"p3","dependency_type":"optional"},
            {"project_id":"p4","dependency_type":"embedded"}
        ]))
        .unwrap();
        let identity = map_identity(raw).unwrap();
        assert_eq!(
            identity
                .dependencies
                .iter()
                .map(|d| d.kind)
                .collect::<Vec<_>>(),
            [
                DependencyKind::Required,
                DependencyKind::Incompatible,
                DependencyKind::Optional,
                DependencyKind::Embedded
            ]
        );
        assert_eq!(
            identity.dependencies[0].version_id.as_deref(),
            Some("pinned")
        );
        assert!(
            serde_json::from_value::<dto::Dependency>(
                json!({"project_id":"p","dependency_type":"suggested"})
            )
            .is_err()
        );
        let missing = serde_json::from_value(json!({"dependency_type":"required"})).unwrap();
        assert!(map_dependency(missing).is_err());
    }

    #[test]
    fn file_references_reject_malformed_urls_and_checksums() {
        let raw = json!({"url":"https://cdn.modrinth.com/file.jar","filename":"file.jar","hashes":{"sha512":"a".repeat(128)},"size":42});
        let file = map_file(serde_json::from_value(raw.clone()).unwrap()).unwrap();
        assert_eq!(file.size, Some(42));
        for url in [
            "http://localhost/a.jar",
            "https://user:password@host/a.jar",
            "file:///tmp/a.jar",
            "https://host/a.jar#fragment",
        ] {
            let mut wrong = raw.clone();
            wrong["url"] = json!(url);
            assert!(map_file(serde_json::from_value(wrong).unwrap()).is_err());
        }
        let mut wrong = raw;
        wrong["hashes"]["sha512"] = json!("bad");
        assert!(map_file(serde_json::from_value(wrong).unwrap()).is_err());
    }

    #[test]
    fn batch_identity_validation_keeps_duplicate_json_keys_visible() {
        let hash = "a".repeat(128);
        let raw = format!(
            r#"{{"{hash}":{{"id":"v1","project_id":"p1","name":"One","version_number":"1"}},"{hash}":{{"id":"v2","project_id":"p2","name":"Two","version_number":"2"}}}}"#
        );
        let response: dto::VersionFilesResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(response.0.len(), 2);
        let requested = HashSet::from([hash.as_str()]);
        let mut seen = HashSet::new();
        validate_batch_result_identity("hash", &response.0[0].0, &requested, &mut seen).unwrap();
        assert!(
            validate_batch_result_identity("hash", &response.0[1].0, &requested, &mut seen)
                .is_err()
        );
        assert!(
            validate_batch_result_identity("hash", &"b".repeat(128), &requested, &mut seen)
                .is_err()
        );
        assert!(validate_unique_hash_inputs(&[hash.clone(), hash]).is_err());
        assert!(
            validate_unique_identity_inputs("project", &["same".into(), "same".into()]).is_err()
        );
    }
}
