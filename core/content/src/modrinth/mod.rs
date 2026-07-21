mod dto;

use crate::error::{ContentError, ContentResult};
use crate::limits::{
    MAX_DETAIL_BODY_BYTES, MAX_PROVIDER_DETAIL_BYTES, MAX_PROVIDER_METADATA_BYTES,
};
use crate::model::{
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentKind, ContentQuery,
    ContentVersion, DependencyKind, FileRef, GalleryImage, LoaderGameFilter, Page, ProjectMetadata,
    ProviderId, ReleaseChannel, SortOrder, VersionIdentity,
};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};

const DEFAULT_BASE_URL: &str = "https://api.modrinth.com/v2";
const MAX_BULK_IDS: usize = 100;
const MAX_PROVIDER_BATCH_ITEMS: usize = 4096;
const MAX_PROVIDER_ID_BYTES: usize = 256;
const USER_AGENT: &str = concat!(
    "mateoltd/axial/",
    env!("CARGO_PKG_VERSION"),
    " (github.com/mateoltd/axial)"
);

#[derive(Debug, Clone)]
pub struct ContentService {
    client: reqwest::Client,
    base_url: String,
}

impl ContentService {
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

    /// Shared connection pool for verified content downloads.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
        max_bytes: usize,
    ) -> ContentResult<T> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(query)
            .send()
            .await?;
        parse_response(response, url, max_bytes).await
    }

    async fn get_json_counted<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
        max_bytes: usize,
    ) -> ContentResult<(T, usize)> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(query)
            .send()
            .await?;
        parse_response_counted(response, url, max_bytes).await
    }
    pub async fn search(&self, query: &ContentQuery) -> ContentResult<Page<CanonicalContent>> {
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

        let response: dto::SearchResponse = self
            .get_json(
                &self.endpoint("/search"),
                &params,
                MAX_PROVIDER_METADATA_BYTES,
            )
            .await?;
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

    pub async fn detail(&self, id: &CanonicalId) -> ContentResult<ContentDetail> {
        let project_id = project_id_of(id)?;
        let project: dto::Project = self
            .get_json(
                &self.endpoint(&format!("/project/{project_id}")),
                &[],
                MAX_PROVIDER_DETAIL_BYTES,
            )
            .await?;
        let versions: Vec<dto::Version> = self
            .get_json(
                &self.endpoint(&format!("/project/{project_id}/version")),
                &[],
                MAX_PROVIDER_METADATA_BYTES,
            )
            .await?;
        map_project_detail(&project_id, project, versions)
    }

    pub async fn versions(
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
                MAX_PROVIDER_METADATA_BYTES,
            )
            .await?;
        map_project_versions(&project_id, versions)
    }

    pub async fn metadata(
        &self,
        ids: &[CanonicalId],
    ) -> ContentResult<HashMap<CanonicalId, ProjectMetadata>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        validate_batch_item_count(ids.len())?;
        let mut requested = HashSet::with_capacity(ids.len());
        let mut project_ids = Vec::with_capacity(ids.len());
        for id in ids {
            let project_id = project_id_of(id)?;
            if !requested.insert(project_id.clone()) {
                return Err(duplicate_batch_input("project"));
            }
            project_ids.push(project_id);
        }
        let mut metadata = HashMap::new();
        let mut seen_results = HashSet::new();
        let mut response_budget = ProviderBatchBudget::new();
        for chunk in project_ids.chunks(MAX_BULK_IDS) {
            let requested_chunk = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
            let (projects, response_bytes): (Vec<dto::Project>, usize) = self
                .get_json_counted(
                    &self.endpoint("/projects"),
                    &[("ids", json_string_array(chunk))],
                    response_budget.remaining(),
                )
                .await?;
            response_budget.admit(response_bytes)?;
            for project in projects {
                validate_batch_result_identity(
                    "project",
                    &project.id,
                    &requested_chunk,
                    &mut seen_results,
                )?;
                let kind = kind_from_project_type(&project.project_type).ok_or_else(|| {
                    ContentError::ProviderMetadataInvalid(
                        "content provider returned an unknown project type".to_string(),
                    )
                })?;
                metadata.insert(
                    CanonicalId::for_project(ProviderId::Modrinth, &project.id),
                    ProjectMetadata {
                        kind,
                        title: project.title,
                    },
                );
            }
        }
        Ok(metadata)
    }

    pub async fn identify(
        &self,
        sha512_hashes: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        if sha512_hashes.is_empty() {
            return Ok(HashMap::new());
        }
        validate_batch_item_count(sha512_hashes.len())?;
        validate_unique_hash_inputs(sha512_hashes)?;
        let url = self.endpoint("/version_files");
        let mut identities = HashMap::new();
        let mut seen_results = HashSet::new();
        let mut response_budget = ProviderBatchBudget::new();
        for chunk in sha512_hashes.chunks(MAX_BULK_IDS) {
            let requested_chunk = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
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
            let (resolved, response_bytes): (dto::VersionFilesResponse, usize) =
                parse_response_counted(response, &url, response_budget.remaining()).await?;
            response_budget.admit(response_bytes)?;
            for (hash, version) in resolved.0 {
                validate_batch_result_identity("hash", &hash, &requested_chunk, &mut seen_results)?;
                identities.insert(hash, map_identity(version)?);
            }
        }
        Ok(identities)
    }

    pub async fn version_identities(
        &self,
        version_ids: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        if version_ids.is_empty() {
            return Ok(HashMap::new());
        }
        validate_batch_item_count(version_ids.len())?;
        validate_unique_identity_inputs("version", version_ids)?;
        let mut identities = HashMap::new();
        let mut seen_results = HashSet::new();
        let mut response_budget = ProviderBatchBudget::new();
        for chunk in version_ids.chunks(MAX_BULK_IDS) {
            let requested_chunk = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
            let (versions, response_bytes): (Vec<dto::Version>, usize) = self
                .get_json_counted(
                    &self.endpoint("/versions"),
                    &[("ids", json_string_array(chunk))],
                    response_budget.remaining(),
                )
                .await?;
            response_budget.admit(response_bytes)?;
            for version in versions {
                validate_batch_result_identity(
                    "version",
                    &version.id,
                    &requested_chunk,
                    &mut seen_results,
                )?;
                let identity = map_identity(version)?;
                identities.insert(identity.version_id.clone(), identity);
            }
        }
        Ok(identities)
    }

    /// Project titles for a batch of IDs, in one round trip.
    pub async fn titles(&self, ids: &[CanonicalId]) -> ContentResult<HashMap<CanonicalId, String>> {
        Ok(self
            .metadata(ids)
            .await?
            .into_iter()
            .map(|(id, metadata)| (id, metadata.title))
            .collect())
    }
}

#[derive(Debug)]
struct ProviderBatchBudget {
    remaining_bytes: usize,
}

impl ProviderBatchBudget {
    fn new() -> Self {
        Self {
            remaining_bytes: MAX_PROVIDER_METADATA_BYTES,
        }
    }

    fn remaining(&self) -> usize {
        self.remaining_bytes
    }

    fn admit(&mut self, bytes: usize) -> ContentResult<()> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes).ok_or_else(|| {
            ContentError::ProviderMetadataInvalid(
                "content provider batch responses exceeded their aggregate size bound".to_string(),
            )
        })?;
        Ok(())
    }
}

async fn parse_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    context: &str,
    max_bytes: usize,
) -> ContentResult<T> {
    parse_response_counted(response, context, max_bytes)
        .await
        .map(|(value, _)| value)
}

async fn parse_response_counted<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    context: &str,
    max_bytes: usize,
) -> ContentResult<(T, usize)> {
    let status = response.status();
    if !status.is_success() {
        return Err(ContentError::Status {
            status,
            context: context.to_string(),
        });
    }
    let bytes = bounded_response_body(response, max_bytes).await?;
    let value = parse_provider_json(&bytes, context)?;
    Ok((value, bytes.len()))
}

async fn bounded_response_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> ContentResult<Vec<u8>> {
    validate_declared_body_length(response.content_length(), max_bytes)?;
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(max_bytes);
    let mut body = Vec::with_capacity(initial_capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        append_bounded_body_chunk(&mut body, &chunk?, max_bytes)?;
    }
    Ok(body)
}

fn validate_declared_body_length(length: Option<u64>, max_bytes: usize) -> ContentResult<()> {
    if length.is_some_and(|length| length > max_bytes as u64) {
        return Err(oversized_provider_response());
    }
    Ok(())
}

fn append_bounded_body_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
) -> ContentResult<()> {
    let retained = max_bytes
        .saturating_add(1)
        .saturating_sub(body.len())
        .min(chunk.len());
    body.extend_from_slice(&chunk[..retained]);
    if body.len() > max_bytes {
        return Err(oversized_provider_response());
    }
    Ok(())
}

fn oversized_provider_response() -> ContentError {
    ContentError::ProviderMetadataInvalid(
        "content provider response exceeded its size bound".to_string(),
    )
}

fn parse_provider_json<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    context: &str,
) -> ContentResult<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        ContentError::ProviderMetadataInvalid(format!(
            "invalid JSON response from {context}: {error}"
        ))
    })
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

fn validate_batch_item_count(count: usize) -> ContentResult<()> {
    if count > MAX_PROVIDER_BATCH_ITEMS {
        return Err(ContentError::Invalid(
            "content provider batch input exceeds its item bound".to_string(),
        ));
    }
    Ok(())
}

fn validate_unique_identity_inputs(label: &str, values: &[String]) -> ContentResult<()> {
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

fn validate_unique_hash_inputs(values: &[String]) -> ContentResult<()> {
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

fn duplicate_batch_input(label: &str) -> ContentError {
    ContentError::Invalid(format!(
        "content provider batch contains a duplicate {label} input"
    ))
}

fn validate_batch_result_identity(
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

fn project_id_of(id: &CanonicalId) -> ContentResult<String> {
    let raw = id.as_str();
    let project = raw
        .strip_prefix("modrinth:")
        .filter(|rest| valid_provider_identity(rest))
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
    })
}

fn map_project_detail(
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

fn map_project_versions(
    requested_project_id: &str,
    versions: Vec<dto::Version>,
) -> ContentResult<Vec<ContentVersion>> {
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

fn map_version(version: dto::Version) -> ContentResult<ContentVersion> {
    validate_provider_identity("version", &version.id)?;
    validate_provider_identity("project", &version.project_id)?;
    Ok(ContentVersion {
        id: version.id,
        name: version.name,
        version_number: version.version_number,
        game_versions: version.game_versions,
        loaders: version.loaders,
        channel: release_channel(&version.version_type)?,
        published: version.date_published,
        downloads: version.downloads,
        files: version.files.into_iter().map(map_file).collect(),
        dependencies: version
            .dependencies
            .into_iter()
            .map(map_dependency)
            .collect::<ContentResult<Vec<_>>>()?,
    })
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

fn map_dependency(dependency: dto::Dependency) -> ContentResult<ContentDependency> {
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

fn map_identity(version: dto::Version) -> ContentResult<VersionIdentity> {
    validate_provider_identity("version", &version.id)?;
    validate_provider_identity("project", &version.project_id)?;
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

fn release_channel(version_type: &str) -> ContentResult<ReleaseChannel> {
    match version_type {
        "release" => Ok(ReleaseChannel::Release),
        "beta" => Ok(ReleaseChannel::Beta),
        "alpha" => Ok(ReleaseChannel::Alpha),
        _ => Err(ContentError::ProviderMetadataInvalid(
            "content version has an unknown release channel".to_string(),
        )),
    }
}

fn validate_provider_identity(label: &str, value: &str) -> ContentResult<()> {
    if !valid_provider_identity(value) {
        return Err(ContentError::ProviderMetadataInvalid(format!(
            "content {label} identity is invalid"
        )));
    }
    Ok(())
}

fn valid_provider_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROVIDER_ID_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_sha512(value: &str) -> bool {
    value.len() == 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn p00_b11_contract_injected_service_maps_provider_neutral_wire_records() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind content service fixture");
        let address = listener.local_addr().expect("content service address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept search request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("read search request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let body = serde_json::to_vec(&serde_json::json!({
                "hits": [{
                    "project_id": "project-a",
                    "slug": "project-a",
                    "title": "Project A",
                    "author": "Author",
                    "description": "Summary",
                    "project_type": "mod"
                }],
                "offset": 0,
                "limit": 1,
                "total_hits": 1
            }))
            .expect("encode search response");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write search headers");
            socket.write_all(&body).await.expect("write search body");
            String::from_utf8(request).expect("search request is UTF-8")
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("content service fixture client");
        let service = ContentService::with_base_url(client, format!("http://{address}/v2"));
        let mut query = ContentQuery::new(ContentKind::Mod);
        query.search = Some("project".to_string());
        query.limit = 1;

        let page = service
            .search(&query)
            .await
            .expect("search through service");
        let request = server.await.expect("search fixture task");
        let wire = serde_json::to_value(&page.items[0]).expect("serialize search hit");

        assert!(request.starts_with("GET /v2/search?"));
        assert_eq!(page.items[0].canonical_id.as_str(), "modrinth:project-a");
        assert_eq!(wire.get("provider"), Some(&serde_json::json!("modrinth")));
        assert!(wire.get("sources").is_none());
    }

    #[tokio::test]
    async fn p00_b11_contract_foreign_namespaces_fail_before_any_request() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind no-request fixture");
        let address = listener.local_addr().expect("no-request address");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("no-request fixture client");
        let service = ContentService::with_base_url(client, format!("http://{address}/v2"));
        let foreign = CanonicalId("curseforge:project-a".to_string());

        assert!(matches!(
            service.detail(&foreign).await,
            Err(ContentError::Invalid(_))
        ));
        assert!(matches!(
            service
                .versions(&foreign, &LoaderGameFilter::default())
                .await,
            Err(ContentError::Invalid(_))
        ));
        assert!(matches!(
            service
                .metadata(&[
                    CanonicalId::for_project(ProviderId::Modrinth, "valid"),
                    foreign,
                ])
                .await,
            Err(ContentError::Invalid(_))
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "foreign IDs must not reach the configured service"
        );
    }

    #[test]
    fn provider_body_limits_admit_exact_and_reject_one_over() {
        validate_declared_body_length(
            Some(MAX_PROVIDER_METADATA_BYTES as u64),
            MAX_PROVIDER_METADATA_BYTES,
        )
        .expect("exact declared metadata limit");
        assert!(
            validate_declared_body_length(
                Some((MAX_PROVIDER_METADATA_BYTES + 1) as u64),
                MAX_PROVIDER_METADATA_BYTES,
            )
            .is_err()
        );
        validate_declared_body_length(
            Some(MAX_PROVIDER_DETAIL_BYTES as u64),
            MAX_PROVIDER_DETAIL_BYTES,
        )
        .expect("exact declared detail limit");
        assert!(
            validate_declared_body_length(
                Some((MAX_PROVIDER_DETAIL_BYTES + 1) as u64),
                MAX_PROVIDER_DETAIL_BYTES,
            )
            .is_err()
        );

        let mut body = Vec::new();
        append_bounded_body_chunk(
            &mut body,
            &vec![0; MAX_PROVIDER_METADATA_BYTES],
            MAX_PROVIDER_METADATA_BYTES,
        )
        .expect("exact streamed metadata limit");
        assert!(append_bounded_body_chunk(&mut body, &[0], MAX_PROVIDER_METADATA_BYTES).is_err());
    }

    #[test]
    fn detail_body_limit_admits_exact_and_rejects_one_over() {
        let project = |body: String| {
            serde_json::from_value::<dto::Project>(serde_json::json!({
                "id": "project",
                "title": "Project",
                "project_type": "mod",
                "body": body
            }))
            .expect("detail project")
        };

        map_project_detail(
            "project",
            project("x".repeat(MAX_DETAIL_BODY_BYTES)),
            Vec::new(),
        )
        .expect("exact detail body limit");
        assert!(
            map_project_detail(
                "project",
                project("x".repeat(MAX_DETAIL_BODY_BYTES + 1)),
                Vec::new(),
            )
            .is_err()
        );
    }

    fn provider_version(id: &str, project_id: &str) -> dto::Version {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "project_id": project_id,
            "name": "Version",
            "version_number": "1.0.0",
            "version_type": "release"
        }))
        .expect("provider version")
    }

    #[test]
    fn project_version_mapping_rejects_wrong_and_duplicate_identities() {
        assert!(map_project_versions("requested", vec![provider_version("v1", "other")]).is_err());
        assert!(
            map_project_versions(
                "requested",
                vec![
                    provider_version("v1", "requested"),
                    provider_version("v1", "requested"),
                ],
            )
            .is_err()
        );

        let wrong_project: dto::Project = serde_json::from_value(serde_json::json!({
            "id": "other",
            "title": "Other",
            "project_type": "mod"
        }))
        .expect("project detail");
        assert!(map_project_detail("requested", wrong_project, Vec::new()).is_err());
    }

    #[test]
    fn batch_inputs_and_aggregate_responses_are_bounded() {
        validate_batch_item_count(MAX_PROVIDER_BATCH_ITEMS).expect("exact batch item limit");
        assert!(validate_batch_item_count(MAX_PROVIDER_BATCH_ITEMS + 1).is_err());
        assert!(
            validate_unique_identity_inputs("version", &["same".to_string(), "same".to_string()])
                .is_err()
        );
        assert!(validate_unique_hash_inputs(&["a".repeat(128), "a".repeat(128)]).is_err());

        let mut budget = ProviderBatchBudget::new();
        budget
            .admit(MAX_PROVIDER_METADATA_BYTES)
            .expect("exact aggregate response limit");
        assert_eq!(budget.remaining(), 0);
        assert!(budget.admit(1).is_err());
    }

    #[test]
    fn batch_results_reject_unexpected_and_duplicate_identities() {
        let requested = HashSet::from(["requested"]);
        let mut seen = HashSet::new();
        assert!(
            validate_batch_result_identity("version", "unexpected", &requested, &mut seen).is_err()
        );
        validate_batch_result_identity("version", "requested", &requested, &mut seen)
            .expect("first requested result");
        assert!(
            validate_batch_result_identity("version", "requested", &requested, &mut seen).is_err()
        );

        let hash = "a".repeat(128);
        let other_hash = "b".repeat(128);
        let requested_hashes = HashSet::from([hash.as_str()]);
        assert!(
            validate_batch_result_identity(
                "hash",
                &other_hash,
                &requested_hashes,
                &mut HashSet::new(),
            )
            .is_err()
        );
        let payload = format!(
            r#"{{"{hash}":{{"id":"v1","project_id":"p1","name":"V1","version_number":"1"}},"{hash}":{{"id":"v2","project_id":"p2","name":"V2","version_number":"2"}}}}"#
        );
        let duplicate_hashes =
            parse_provider_json::<dto::VersionFilesResponse>(payload.as_bytes(), "hash fixture")
                .expect("preserve duplicate JSON keys");
        assert_eq!(duplicate_hashes.0.len(), 2);
        let mut seen_hashes = HashSet::new();
        for (hash, _) in duplicate_hashes.0 {
            if validate_batch_result_identity("hash", &hash, &requested_hashes, &mut seen_hashes)
                .is_err()
            {
                return;
            }
        }
        panic!("duplicate hash result must be rejected");
    }

    #[test]
    fn invalid_provider_json_is_typed_as_metadata_invalid() {
        let error = parse_provider_json::<serde_json::Value>(b"{", "provider endpoint")
            .expect_err("invalid provider JSON must fail closed");

        assert!(matches!(error, ContentError::ProviderMetadataInvalid(_)));
    }

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

    #[test]
    fn dependency_mapping_rejects_unknown_and_identityless_records() {
        let unknown = serde_json::from_value::<dto::Version>(serde_json::json!({
            "id": "version-a",
            "project_id": "project-a",
            "name": "Project A",
            "version_number": "1.0.0",
            "dependencies": [{ "project_id": "dependency", "dependency_type": "suggested" }]
        }));
        assert!(
            unknown.is_err(),
            "unknown dependency types must fail decoding"
        );

        let identityless: dto::Version = serde_json::from_value(serde_json::json!({
            "id": "version-a",
            "project_id": "project-a",
            "name": "Project A",
            "version_number": "1.0.0",
            "dependencies": [{ "dependency_type": "required" }]
        }))
        .expect("structurally decoded identityless dependency");
        assert!(map_identity(identityless).is_err());
    }

    #[test]
    fn version_mapping_rejects_unknown_release_channels() {
        let version: dto::Version = serde_json::from_value(serde_json::json!({
            "id": "version-a",
            "project_id": "project-a",
            "name": "Project A",
            "version_number": "1.0.0",
            "version_type": "candidate"
        }))
        .expect("version payload");

        assert!(map_version(version).is_err());
    }
}
