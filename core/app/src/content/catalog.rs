//! Fresh Modrinth reads for discovery, dependency resolution and pack provenance.

use super::model::*;
use super::provider::*;
use crate::network::{
    DownloadError, DownloadRequest, IntegrityPolicy, OriginPolicy, ProviderClient, ResponseLimits,
    UnhashedReason,
};
use crate::tasks::CancellationToken;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};

const DEFAULT_BASE_URL: &str = "https://api.modrinth.com/v2/";
const MAX_BULK_IDS: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("content metadata could not be decoded: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("content provider request failed: {0}")]
    Provider(#[from] DownloadError),
    #[error("content provider metadata was not valid: {0}")]
    ProviderMetadataInvalid(String),
    #[error("content request was invalid: {0}")]
    Invalid(String),
    #[error("content is not available for the requested loader or game version")]
    Unavailable,
}

pub type ContentResult<T> = Result<T, ContentError>;

#[derive(Clone)]
pub struct ContentService {
    client: ProviderClient,
    base_url: reqwest::Url,
    origins: OriginPolicy,
    cancellation: CancellationToken,
}

impl ContentService {
    pub fn new(client: ProviderClient) -> ContentResult<Self> {
        Self::with_base_url(
            client,
            DEFAULT_BASE_URL,
            OriginPolicy::https(["https://api.modrinth.com"], 3)?,
        )
    }

    /// The composition owner supplies an admitted provider origin. Tests use
    /// the network owner's loopback policy with a real local HTTP fixture.
    pub fn with_base_url(
        client: ProviderClient,
        base_url: impl AsRef<str>,
        origins: OriginPolicy,
    ) -> ContentResult<Self> {
        let base_url = format!("{}/", base_url.as_ref().trim_end_matches('/'));
        let base_url = reqwest::Url::parse(&base_url)
            .map_err(|_| ContentError::Invalid("invalid content provider endpoint".into()))?;
        if !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.fragment().is_some()
            || base_url.query().is_some()
        {
            return Err(ContentError::Invalid(
                "invalid content provider endpoint".into(),
            ));
        }
        Ok(Self {
            client,
            base_url,
            origins,
            cancellation: CancellationToken::new(),
        })
    }

    /// Bind a retained content operation to its own cancellation request.
    pub fn with_cancellation(&self, cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            ..self.clone()
        }
    }

    fn endpoint(&self, path: &str, query: &[(&str, String)]) -> ContentResult<String> {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|_| ContentError::Invalid("invalid content provider endpoint".into()))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        }
        Ok(url.to_string())
    }

    async fn read<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
        max_bytes: usize,
    ) -> ContentResult<(T, usize)> {
        if max_bytes == 0 {
            return Err(ContentError::ProviderMetadataInvalid(
                "content provider batch exceeded its aggregate size bound".into(),
            ));
        }
        let url = self.endpoint(path, query)?;
        let limits = ResponseLimits::new(max_bytes as u64, max_bytes as u64);
        let integrity = IntegrityPolicy::Unhashed {
            reason: UnhashedReason::ProviderMetadata,
            expected_size: None,
        };
        let request = match body {
            Some(body) => {
                DownloadRequest::post_json(url, body, self.origins.clone(), limits, integrity)
            }
            None => DownloadRequest::get(url, self.origins.clone(), limits, integrity),
        };
        let response = Box::pin(self.client.fetch(request, &self.cancellation)).await?;
        let bytes = response.bytes();
        let value = serde_json::from_slice(bytes).map_err(|_| {
            ContentError::ProviderMetadataInvalid("content provider returned malformed JSON".into())
        })?;
        Ok((value, bytes.len()))
    }

    pub async fn search(&self, query: &ContentQuery) -> ContentResult<Page<CanonicalContent>> {
        validate_query(query)?;
        let mut params = vec![
            ("index", sort_index(query.sort).to_string()),
            ("offset", query.offset.to_string()),
            ("limit", query.limit.clamp(1, 100).to_string()),
            ("facets", build_facets(query)),
        ];
        if let Some(search) = query
            .search
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            params.push(("query", search.clone()));
        }
        let (response, _) = self
            .read("search", &params, None, MAX_PROVIDER_METADATA_BYTES)
            .await?;
        map_search_page(query, response)
    }

    pub async fn detail(&self, id: &CanonicalId) -> ContentResult<ContentDetail> {
        let project = project_id_of(id)?;
        let (metadata, _) = self
            .read(
                &format!("project/{project}"),
                &[],
                None,
                MAX_PROVIDER_DETAIL_BYTES,
            )
            .await?;
        let (versions, _) = self
            .read(
                &format!("project/{project}/version"),
                &[],
                None,
                MAX_PROVIDER_METADATA_BYTES,
            )
            .await?;
        map_project_detail(&project, metadata, versions)
    }

    pub async fn versions(
        &self,
        id: &CanonicalId,
        filter: &LoaderGameFilter,
    ) -> ContentResult<Vec<ContentVersion>> {
        let project = project_id_of(id)?;
        validate_filter(filter)?;
        let mut params = Vec::new();
        for (name, value) in [
            ("loaders", &filter.loader),
            ("game_versions", &filter.game_version),
        ] {
            if let Some(value) = value.as_ref().filter(|value| !value.is_empty()) {
                params.push((name, json_string_array(std::slice::from_ref(value))));
            }
        }
        let (versions, _) = self
            .read(
                &format!("project/{project}/version"),
                &params,
                None,
                MAX_PROVIDER_METADATA_BYTES,
            )
            .await?;
        let versions = map_project_versions(&project, versions)?;
        Ok(versions
            .into_iter()
            .filter(|version| {
                filter
                    .loader
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .is_none_or(|loader| version.loaders.contains(loader))
                    && filter
                        .game_version
                        .as_ref()
                        .filter(|value| !value.is_empty())
                        .is_none_or(|game| version.game_versions.contains(game))
            })
            .collect())
    }

    /// Pinned dependencies must be fetched without compatibility filters, then
    /// checked against their actual project and target by the resolver.
    pub async fn version(&self, version_id: &str) -> ContentResult<(CanonicalId, ContentVersion)> {
        validate_unique_identity_inputs("version", &[version_id.to_string()])?;
        let (version, _): (dto::Version, usize) = self
            .read(
                &format!("version/{version_id}"),
                &[],
                None,
                MAX_PROVIDER_METADATA_BYTES,
            )
            .await?;
        if version.id != version_id {
            return Err(ContentError::ProviderMetadataInvalid(
                "content provider returned a different version".into(),
            ));
        }
        let project = CanonicalId::for_project(ProviderId::Modrinth, &version.project_id);
        Ok((project, map_version(version)?))
    }

    pub async fn metadata(
        &self,
        ids: &[CanonicalId],
    ) -> ContentResult<HashMap<CanonicalId, ProjectMetadata>> {
        validate_batch_item_count(ids.len())?;
        let projects = ids
            .iter()
            .map(project_id_of)
            .collect::<ContentResult<Vec<_>>>()?;
        validate_unique_identity_inputs("project", &projects)?;
        let mut result = HashMap::new();
        let mut seen = HashSet::new();
        let mut budget = ProviderBatchBudget::new();
        for chunk in projects.chunks(MAX_BULK_IDS) {
            let requested = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
            let (projects, bytes): (Vec<dto::Project>, usize) = self
                .read(
                    "projects",
                    &[("ids", json_string_array(chunk))],
                    None,
                    budget.remaining(),
                )
                .await?;
            budget.admit(bytes)?;
            for project in projects {
                validate_batch_result_identity("project", &project.id, &requested, &mut seen)?;
                let kind = kind_from_project_type(&project.project_type).ok_or_else(|| {
                    ContentError::ProviderMetadataInvalid("unknown content project type".into())
                })?;
                result.insert(
                    CanonicalId::for_project(ProviderId::Modrinth, &project.id),
                    ProjectMetadata {
                        kind,
                        title: project.title,
                    },
                );
            }
        }
        Ok(result)
    }

    pub async fn identify(
        &self,
        hashes: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        validate_batch_item_count(hashes.len())?;
        validate_unique_hash_inputs(hashes)?;
        let mut result = HashMap::new();
        let mut seen = HashSet::new();
        let mut budget = ProviderBatchBudget::new();
        for chunk in hashes.chunks(MAX_BULK_IDS) {
            let requested = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
            let body = serde_json::json!({"hashes": chunk, "algorithm": "sha512"});
            let (versions, bytes): (dto::VersionFilesResponse, usize) = self
                .read("version_files", &[], Some(body), budget.remaining())
                .await?;
            budget.admit(bytes)?;
            for (hash, version) in versions.0 {
                validate_batch_result_identity("hash", &hash, &requested, &mut seen)?;
                if !version
                    .files
                    .iter()
                    .any(|file| file.hashes.sha512.as_deref() == Some(hash.as_str()))
                {
                    return Err(ContentError::ProviderMetadataInvalid(
                        "content hash identity has no matching published file".into(),
                    ));
                }
                result.insert(hash, map_identity(version)?);
            }
        }
        Ok(result)
    }

    pub async fn version_identities(
        &self,
        ids: &[String],
    ) -> ContentResult<HashMap<String, VersionIdentity>> {
        validate_batch_item_count(ids.len())?;
        validate_unique_identity_inputs("version", ids)?;
        let mut result = HashMap::new();
        let mut seen = HashSet::new();
        let mut budget = ProviderBatchBudget::new();
        for chunk in ids.chunks(MAX_BULK_IDS) {
            let requested = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
            let (versions, bytes): (Vec<dto::Version>, usize) = self
                .read(
                    "versions",
                    &[("ids", json_string_array(chunk))],
                    None,
                    budget.remaining(),
                )
                .await?;
            budget.admit(bytes)?;
            for version in versions {
                validate_batch_result_identity("version", &version.id, &requested, &mut seen)?;
                result.insert(version.id.clone(), map_identity(version)?);
            }
        }
        Ok(result)
    }

    pub async fn titles(&self, ids: &[CanonicalId]) -> ContentResult<HashMap<CanonicalId, String>> {
        Ok(self
            .metadata(ids)
            .await?
            .into_iter()
            .map(|(id, metadata)| (id, metadata.title))
            .collect())
    }
}

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
                "content provider batch exceeded its aggregate size bound".into(),
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::ClientConfig;
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    async fn fixture(responses: Vec<(u16, String)>) -> (ContentService, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let origins = OriginPolicy::loopback_for_tests([&origin], 3).unwrap();
        let service =
            ContentService::with_base_url(client, format!("{origin}/v2"), origins).unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    assert!(request.len() < 1024 * 1024);
                    if let Some(header_end) =
                        request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let header = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
            }
            requests
        });
        (service, server)
    }

    fn response(value: Value) -> (u16, String) {
        (200, value.to_string())
    }

    fn published_version(project: &str, id: &str) -> Value {
        json!({"project_id":project,"id":id,"name":"Release","version_number":"1.0",
            "version_type":"release","loaders":["fabric"],"game_versions":["1.21.6"],
            "files":[{"filename":"content.jar","url":"https://cdn.modrinth.com/content.jar",
                "size":42,"primary":true,"hashes":{"sha512":"a".repeat(128)}}],
            "dependencies":[{"version_id":"dependency-pin","dependency_type":"required"}]})
    }

    fn request_url(request: &str) -> reqwest::Url {
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        reqwest::Url::parse(&format!("http://localhost{target}")).unwrap()
    }

    #[tokio::test]
    async fn search_encodes_user_input_and_retains_page_identity() {
        let (service, server) = fixture(vec![response(json!({
            "hits":[{"project_id":"p1","title":"Sodium","project_type":"mod"}],
            "offset":10,"limit":1,"total_hits":20
        }))])
        .await;
        let mut query = ContentQuery::new(ContentKind::Mod);
        query.search = Some("foo & 日本語".into());
        query.offset = 10;
        query.limit = 1;
        query.loader = Some("fabric".into());
        query.game_version = Some("1.21.6".into());
        let page = service.search(&query).await.unwrap();
        assert_eq!(page.items[0].canonical_id.as_str(), "modrinth:p1");
        assert_eq!((page.offset, page.limit, page.total), (10, 1, 20));
        let requests = server.await.unwrap();
        let url = request_url(&requests[0]);
        assert_eq!(url.path(), "/v2/search");
        let params = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(params["query"], "foo & 日本語");
        assert_eq!(
            serde_json::from_str::<Value>(&params["facets"]).unwrap(),
            json!([
                ["project_type:mod"],
                ["categories:fabric"],
                ["versions:1.21.6"]
            ])
        );
    }

    #[tokio::test]
    async fn detail_versions_and_exact_pin_use_the_real_provider_boundary() {
        let version = published_version("p1", "v1");
        let mut wrong_loader = published_version("p1", "v2");
        wrong_loader["loaders"] = json!(["forge"]);
        let (service, server) = fixture(vec![
            response(
                json!({"id":"p1","title":"Project","project_type":"mod","body":"Description",
                "gallery":[{"url":"https://cdn.modrinth.com/gallery.png","title":"Scene"}]}),
            ),
            response(json!([version.clone()])),
            response(json!([version.clone(), wrong_loader])),
            response(version),
        ])
        .await;
        let id = CanonicalId("modrinth:p1".into());
        let detail = service.detail(&id).await.unwrap();
        assert_eq!(detail.body, "Description");
        assert_eq!(detail.gallery[0].title.as_deref(), Some("Scene"));
        assert_eq!(
            detail.versions[0].primary_file().unwrap().filename,
            "content.jar"
        );
        let filtered = service
            .versions(
                &id,
                &LoaderGameFilter {
                    loader: Some("fabric".into()),
                    game_version: Some("1.21.6".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            filtered
                .iter()
                .map(|version| version.id.as_str())
                .collect::<Vec<_>>(),
            ["v1"]
        );
        let (actual_project, pinned) = service.version("v1").await.unwrap();
        assert_eq!(actual_project, id);
        assert_eq!(
            pinned.dependencies[0].version_id.as_deref(),
            Some("dependency-pin")
        );
        let requests = server.await.unwrap();
        let versions_url = request_url(&requests[2]);
        let params = versions_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(params["loaders"], "[\"fabric\"]");
        assert_eq!(params["game_versions"], "[\"1.21.6\"]");
        assert_eq!(request_url(&requests[3]).path(), "/v2/version/v1");
    }

    #[tokio::test]
    async fn provider_hash_lookup_preserves_file_identity_and_version_dependencies() {
        let hash = "a".repeat(128);
        let version = published_version("p1", "v1");
        let (service, server) = fixture(vec![
            response(json!({&hash: version.clone()})),
            response(json!([version])),
            response(json!([{"id":"p1","title":"Project","project_type":"shader"}])),
        ])
        .await;
        let identities = service.identify(std::slice::from_ref(&hash)).await.unwrap();
        assert_eq!(identities[&hash].project_id, "p1");
        assert_eq!(
            identities[&hash].dependencies[0].version_id.as_deref(),
            Some("dependency-pin")
        );
        assert_eq!(
            service.version_identities(&["v1".into()]).await.unwrap()["v1"].version_id,
            "v1"
        );
        let id = CanonicalId("modrinth:p1".into());
        let metadata = service.metadata(std::slice::from_ref(&id)).await.unwrap();
        assert_eq!(metadata[&id].kind, ContentKind::ShaderPack);
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /v2/version_files "));
        let body: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body, json!({"hashes":[hash],"algorithm":"sha512"}));
    }

    #[tokio::test]
    async fn hash_lookup_refuses_a_version_without_the_requested_file() {
        let hash = "b".repeat(128);
        let (service, server) = fixture(vec![response(
            json!({&hash: published_version("p1", "v1")}),
        )])
        .await;
        assert!(matches!(
            service.identify(&[hash]).await,
            Err(ContentError::ProviderMetadataInvalid(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn batches_chunk_at_one_hundred_and_do_not_accept_foreign_results() {
        let ids = (0..101)
            .map(|index| CanonicalId(format!("modrinth:p{index}")))
            .collect::<Vec<_>>();
        let (service, server) = fixture(vec![response(json!([])), response(json!([]))]).await;
        assert!(service.metadata(&ids).await.unwrap().is_empty());
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        for (request, expected_count) in requests.iter().zip([100, 1]) {
            let url = request_url(request);
            let ids = url.query_pairs().find(|(key, _)| key == "ids").unwrap().1;
            assert_eq!(
                serde_json::from_str::<Vec<String>>(&ids).unwrap().len(),
                expected_count
            );
        }
        let (service, server) = fixture(vec![response(
            json!([{"id":"foreign","title":"Foreign","project_type":"mod"}]),
        )])
        .await;
        assert!(matches!(
            service.metadata(&ids[..1]).await,
            Err(ContentError::ProviderMetadataInvalid(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_provider_data_and_http_failures_never_become_empty_success() {
        let (service, server) = fixture(vec![
            (200, "{".into()),
            (429, "sensitive upstream failure".into()),
            response(published_version("p1", "different")),
        ])
        .await;
        assert!(matches!(
            service.search(&ContentQuery::new(ContentKind::Mod)).await,
            Err(ContentError::ProviderMetadataInvalid(_))
        ));
        let error = service
            .search(&ContentQuery::new(ContentKind::Mod))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ContentError::Provider(DownloadError::HttpStatus { status: 429 })
        ));
        assert!(!error.to_string().contains("sensitive"));
        assert!(matches!(
            service.version("v1").await,
            Err(ContentError::ProviderMetadataInvalid(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_input_and_cancelled_reads_never_issue_provider_requests() {
        let (service, server) = fixture(vec![]).await;
        let id = CanonicalId("modrinth:../other".into());
        assert!(matches!(
            service.detail(&id).await,
            Err(ContentError::Invalid(_))
        ));
        assert!(matches!(
            service.versions(&id, &LoaderGameFilter::default()).await,
            Err(ContentError::Invalid(_))
        ));
        assert!(matches!(
            service.metadata(&[id]).await,
            Err(ContentError::Invalid(_))
        ));
        assert!(matches!(
            service.identify(&["bad".into()]).await,
            Err(ContentError::Invalid(_))
        ));
        assert!(matches!(
            service
                .version_identities(&["same".into(), "same".into()])
                .await,
            Err(ContentError::Invalid(_))
        ));
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = service.with_cancellation(cancellation);
        assert!(matches!(
            cancelled.version("v1").await,
            Err(ContentError::Provider(DownloadError::Cancelled))
        ));
        assert!(server.await.unwrap().is_empty());
    }

    #[test]
    fn aggregate_batch_budget_does_not_reset_between_requests() {
        let mut budget = ProviderBatchBudget::new();
        budget.admit(MAX_PROVIDER_METADATA_BYTES - 1).unwrap();
        assert_eq!(budget.remaining(), 1);
        budget.admit(1).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert!(budget.admit(1).is_err());
    }

    #[tokio::test]
    #[ignore = "performs read-only requests against the live Modrinth API"]
    async fn live_modrinth_search_detail_versions_and_hash_identity_roundtrip() {
        let service =
            ContentService::new(ProviderClient::new(ClientConfig::default()).unwrap()).unwrap();
        let mut query = ContentQuery::new(ContentKind::Mod);
        query.search = Some("sodium".into());
        query.loader = Some("fabric".into());
        query.game_version = Some("1.21.6".into());
        query.limit = 5;
        let page = service.search(&query).await.unwrap();
        assert!(!page.items.is_empty());
        let id = CanonicalId("modrinth:AANobbMI".into());
        let detail = service.detail(&id).await.unwrap();
        assert_eq!(detail.content.canonical_id, id);
        assert_eq!(detail.content.title.to_lowercase(), "sodium");
        let versions = service
            .versions(
                &id,
                &LoaderGameFilter {
                    loader: Some("fabric".into()),
                    game_version: Some("1.21.6".into()),
                },
            )
            .await
            .unwrap();
        let chosen = versions.first().expect("Sodium for Fabric 1.21.6");
        let hash = chosen.primary_file().unwrap().sha512.as_ref().unwrap();
        let identity = service.identify(std::slice::from_ref(hash)).await.unwrap();
        assert_eq!(identity[hash].project_id, "AANobbMI");
        assert_eq!(identity[hash].version_id, chosen.id);
    }
}
