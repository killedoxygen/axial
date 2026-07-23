//! Content discovery orchestration: search and browse upstream content, resolve
//! a backend-authored install plan against a target, and install verified files
//! into an instance. Modrinth access and mapping, verified download, the
//! provenance manifest, and modpack import live in `axial-content`; this module
//! adapts them to the HTTP surface and keeps policy — dependency resolution,
//! conflict detection, compatibility ranking — on the backend.
//!
//! A target is either an instance that exists or a draft one the user is about
//! to create, so browsing before you own anything and adding to a library you
//! already have are the same code path.

pub mod compat;
mod operation;
pub mod pack;
pub mod resolve;
pub mod target;

use crate::application::instances::handle_create_instance_view;
use crate::application::{
    InstallQueueContentActionRequest, InstallQueueContentSelection, InstallQueueRequest,
    InstallQueueStateResponse, enqueue_install_owned, enqueue_install_with_dependency_admitted,
    filesystem::run_blocking_filesystem,
};
use crate::state::{
    AppState, InstanceLifecycleLease, ProducerLease, RequestProducerHandoff, UpdateOperationLease,
};
use axial_content::{
    CanonicalContent, CanonicalId, ContentDetail, ContentError, ContentKind, ContentManifest,
    ContentQuery, ContentVersion, LiveManagedContent, ManifestEntry, Page, ProviderId, SortOrder,
    canonicalize_version_only_dependencies, entry_file_present,
    has_unresolved_version_only_incompatibility, newer_version, version_conflicts_with_installed,
    version_matches_filter,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use compat::{CompatCandidate, CompatDrop};
pub(crate) use pack::queue_modpack_install_after_admitted;
pub(crate) use pack::validate_modpack_file_selection_ids;
pub(crate) use operation::{
    ContentOperationTask, start_content_install_task, start_content_uninstall_task,
    start_modpack_install_task,
};
pub use pack::{
    ModpackFileOption, ModpackFilesPlan, ModpackInstallRequest, ModpackInstallResponse,
    ModpackTarget, modpack_files,
};
pub use resolve::{ConflictKind, PlanConflict, PlanItem, PlanReason, ResolutionPlan};
pub use target::TargetRef;

use futures_util::{StreamExt, stream};
use resolve::{into_plan, resolve};
use target::{require_instance_game_dir, resolve_target};

pub type ContentApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentExecutionFailureKind {
    FileOperation,
    MetadataInvalid,
    NetworkFailure,
    PermissionDenied,
    ProviderFailure,
}

pub(crate) struct ContentExecutionError {
    response: ContentApiError,
    failure_kind: Option<ContentExecutionFailureKind>,
}

impl ContentExecutionError {
    pub(crate) fn into_parts(self) -> (ContentApiError, Option<ContentExecutionFailureKind>) {
        (self.response, self.failure_kind)
    }
}

impl From<ContentApiError> for ContentExecutionError {
    fn from(response: ContentApiError) -> Self {
        Self {
            response,
            failure_kind: None,
        }
    }
}

const DEFAULT_SEARCH_LIMIT: u32 = 40;
const MAX_SEARCH_LIMIT: u32 = 100;
const MAX_COMPAT_ITEMS: usize = 40;
const COMPAT_DETAIL_CONCURRENCY: usize = 6;
const MAX_CONFLICT_ERROR_CHARS: usize = 1024;

#[derive(Debug, Deserialize)]
pub struct ContentSearchParams {
    pub kind: ContentKind,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub loader: Option<String>,
    #[serde(default)]
    pub game_version: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub sort: Option<SortOrder>,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// When set, every result is annotated with what this instance already has.
    #[serde(default)]
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContentSelection {
    pub canonical_id: String,
    pub kind: ContentKind,
    #[serde(default)]
    pub version_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContentPlanRequest {
    pub target: TargetRef,
    pub selections: Vec<ContentSelection>,
}

#[derive(Debug, Deserialize)]
pub struct ContentInstallRequest {
    pub instance_id: String,
    pub selections: Vec<ContentSelection>,
    /// Proceed past declared incompatibilities. Unavailable items are never
    /// installable, override or not.
    #[serde(default)]
    pub allow_incompatible: bool,
}

#[derive(Debug, Deserialize)]
pub struct ContentCompatRequest {
    pub selections: Vec<ContentSelection>,
}

#[derive(Debug, Serialize)]
pub struct ContentCompatResponse {
    pub candidates: Vec<CompatCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_view: Option<serde_json::Value>,
}

/// What a target instance already has of a search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Installed,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub content: CanonicalContent,
    /// Absent when browsing without a target instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_state: Option<InstallState>,
}

#[derive(Debug, Serialize)]
pub struct InstanceContentEntry {
    pub canonical_id: CanonicalId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub kind: ContentKind,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct InstanceContentResponse {
    pub entries: Vec<InstanceContentEntry>,
}

#[derive(Debug, Serialize)]
pub struct ContentUpdate {
    pub canonical_id: CanonicalId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub kind: ContentKind,
    pub current_version_id: String,
    pub latest_version_id: String,
    pub latest_version_number: String,
}

#[derive(Debug, Serialize)]
pub struct ContentUpdatesResponse {
    pub updates: Vec<ContentUpdate>,
}

pub async fn content_search(
    state: &AppState,
    params: ContentSearchParams,
) -> Result<Page<SearchHit>, ContentApiError> {
    let mut query = ContentQuery::new(params.kind);
    query.search = params.query.filter(|value| !value.trim().is_empty());
    query.loader = params.loader.filter(|value| !value.is_empty());
    query.game_version = params.game_version.filter(|value| !value.is_empty());
    if let Some(category) = params.category.filter(|value| !value.is_empty()) {
        query.categories = vec![category];
    }
    if let Some(sort) = params.sort {
        query.sort = sort;
    }
    query.offset = params.offset.unwrap_or(0);
    query.limit = params
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);

    let page = state
        .content()
        .search(&query)
        .await
        .map_err(content_error_response)?;

    // Annotation is best-effort: a search still returns results for an instance
    // whose manifest cannot be read. Presence is checked under the same lock as
    // content mutations so a stale manifest entry never hides the Add action.
    let candidate_ids: HashSet<CanonicalId> = page
        .items
        .iter()
        .map(|content| content.canonical_id.clone())
        .collect();
    let installed_ids = if let Some(instance_id) = params.instance_id.as_deref() {
        if let Ok(game_dir) = require_instance_game_dir(state, instance_id) {
            if let Some(_lifecycle_guard) = state.try_acquire_instance_lifecycle(instance_id).await
            {
                match load_ambient_content_snapshot(game_dir, Some(candidate_ids)).await {
                    Ok((manifest, live_content)) => {
                        present_installed_ids(&manifest, &live_content)
                    }
                    Err(_) => HashSet::new(),
                }
            } else {
                HashSet::new()
            }
        } else {
            HashSet::new()
        }
    } else {
        HashSet::new()
    };

    Ok(project_search_page(page, &installed_ids))
}

fn project_search_page(
    page: Page<CanonicalContent>,
    installed_ids: &HashSet<CanonicalId>,
) -> Page<SearchHit> {
    Page {
        items: page
            .items
            .into_iter()
            .map(|content| SearchHit {
                install_state: installed_ids
                    .contains(&content.canonical_id)
                    .then_some(InstallState::Installed),
                content,
            })
            .collect(),
        offset: page.offset,
        limit: page.limit,
        total: page.total,
    }
}

fn present_installed_ids(
    manifest: &ContentManifest,
    live_content: &LiveManagedContent,
) -> HashSet<CanonicalId> {
    manifest
        .entries()
        .iter()
        .filter(|entry| live_content.contains(entry))
        .map(|entry| entry.canonical_id().clone())
        .collect()
}

pub(super) async fn load_ambient_content_snapshot(
    game_dir: PathBuf,
    candidate_ids: Option<HashSet<CanonicalId>>,
) -> Result<(ContentManifest, LiveManagedContent), ContentError> {
    run_blocking_filesystem(move || {
        let manifest = ContentManifest::load(&game_dir)?;
        let live_content =
            project_ambient_live_content(&game_dir, &manifest, candidate_ids.as_ref());
        Ok((manifest, live_content))
    })
    .await
    .map_err(|_| {
        ContentError::Io(std::io::Error::other(
            "content liveness filesystem worker stopped",
        ))
    })?
}

fn project_ambient_live_content(
    game_dir: &Path,
    manifest: &ContentManifest,
    candidate_ids: Option<&HashSet<CanonicalId>>,
) -> LiveManagedContent {
    LiveManagedContent::from_entries(
        manifest
            .entries()
            .iter()
            .filter(|entry| {
                candidate_ids
                    .map(|ids| ids.contains(entry.canonical_id()))
                    .unwrap_or(true)
            })
            .filter(|entry| entry_file_present(game_dir, entry)),
    )
}

pub async fn content_detail(
    state: &AppState,
    canonical_id: &str,
) -> Result<ContentDetail, ContentApiError> {
    let id = CanonicalId(canonical_id.to_string());
    state
        .content()
        .detail(&id)
        .await
        .map_err(content_error_response)
}

pub async fn content_plan(
    state: &AppState,
    request: ContentPlanRequest,
) -> Result<ResolutionPlan, ContentApiError> {
    let target = resolve_target(state, &request.target).await?;

    // A draft target has nothing installed, so it plans against an empty
    // manifest and every item reads as fresh.
    let (manifest, live_content) = match target.game_dir() {
        Some(game_dir) => load_ambient_content_snapshot(game_dir.to_path_buf(), None)
            .await
            .map_err(content_error_response)?,
        None => (ContentManifest::default(), LiveManagedContent::default()),
    };
    let resolution = resolve(
        state,
        target.resolution(),
        &request.selections,
        &live_content,
    )
    .await?;

    let instance_id = match &request.target {
        TargetRef::Instance { instance_id } => Some(instance_id.clone()),
        TargetRef::Draft { .. } => None,
    };
    Ok(into_plan(resolution, instance_id, target.resolution()))
}

pub(crate) async fn queue_content_install(
    state: &AppState,
    request: ContentInstallRequest,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    let count = request.selections.len();
    enqueue_install_owned(
        state,
        content_install_queue_request(request, count),
        handoff,
    )
    .await
}

pub(crate) async fn queue_content_install_with_cleanup_after_admitted(
    state: &AppState,
    request: ContentInstallRequest,
    setup_cleanup: Option<crate::state::SetupInstanceCleanup>,
    prerequisite_queue_id: Option<String>,
    producer: ProducerLease,
    update_admission: UpdateOperationLease,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    let count = request.selections.len();
    enqueue_install_with_dependency_admitted(
        state,
        content_install_queue_request(request, count),
        prerequisite_queue_id,
        setup_cleanup,
        producer,
        update_admission,
    )
    .await
}

fn content_install_queue_request(
    request: ContentInstallRequest,
    count: usize,
) -> InstallQueueRequest {
    InstallQueueRequest::Content {
        instance_id: request.instance_id,
        label: match count {
            1 => "Adding content".to_string(),
            count => format!("Adding {count} content items"),
        },
        action: InstallQueueContentActionRequest::Install {
            selections: request
                .selections
                .into_iter()
                .map(|selection| InstallQueueContentSelection {
                    canonical_id: selection.canonical_id,
                    kind: selection.kind,
                    version_id: selection.version_id,
                })
                .collect(),
            allow_incompatible: request.allow_incompatible,
        },
    }
}

/// Which instances a staged set of content could live in. Drives the flow where
/// someone picks content before they have anywhere to put it.
pub(crate) async fn content_compatibility(
    state: &AppState,
    producer: &ProducerLease,
    request: ContentCompatRequest,
) -> Result<ContentCompatResponse, ContentApiError> {
    if request.selections.is_empty() {
        return Ok(ContentCompatResponse {
            candidates: Vec::new(),
            create_view: None,
        });
    }
    if request.selections.len() > MAX_COMPAT_ITEMS {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "too many items selected at once",
        ));
    }

    let item_results = stream::iter(request.selections.into_iter().map(|selection| async move {
        let id = CanonicalId(selection.canonical_id);
        let detail = state
            .content()
            .detail(&id)
            .await
            .map_err(content_error_response)?;
        let versions = detail
            .versions
            .into_iter()
            .filter(|version| {
                selection
                    .version_id
                    .as_deref()
                    .is_none_or(|selected| version.id == selected)
            })
            .map(|version| {
                let installable = version.primary_file().is_some();
                compat::CompatVersion::from_provider(
                    version.loaders,
                    version.game_versions,
                    installable,
                )
            })
            .collect();
        Ok(compat::CompatItem {
            canonical_id: id,
            title: detail.content.title,
            kind: detail.content.kind,
            versions,
        })
    }))
    .buffered(COMPAT_DETAIL_CONCURRENCY)
    .collect::<Vec<Result<compat::CompatItem, ContentApiError>>>()
    .await;
    let items = item_results.into_iter().collect::<Result<Vec<_>, _>>()?;

    let candidates = compat::rank_candidates(&items);
    let create_view = if let Some(best) = candidates.first() {
        let source = best
            .component_id()
            .map(|component| component.as_str())
            .unwrap_or("vanilla");
        Some(
            serde_json::to_value(handle_create_instance_view(state, producer, Some(source)).await)
                .expect("create view should serialize"),
        )
    } else {
        None
    };

    Ok(ContentCompatResponse {
        candidates,
        create_view,
    })
}

pub(crate) async fn queue_content_uninstall(
    state: &AppState,
    instance_id: &str,
    canonical_id: &str,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    queue_content_uninstalls(state, instance_id, vec![canonical_id.to_string()], handoff).await
}

pub(crate) async fn queue_content_uninstalls(
    state: &AppState,
    instance_id: &str,
    canonical_ids: Vec<String>,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, ContentApiError> {
    enqueue_install_owned(
        state,
        InstallQueueRequest::Content {
            instance_id: instance_id.to_string(),
            label: "Removing content".to_string(),
            action: InstallQueueContentActionRequest::Uninstall { canonical_ids },
        },
        handoff,
    )
    .await
}

async fn lock_instance_for_content_mutation(
    state: &AppState,
    instance_id: &str,
) -> Result<InstanceLifecycleLease, ContentApiError> {
    state
        .try_acquire_instance_lifecycle(instance_id)
        .await
        .ok_or_else(|| {
            json_error(
                StatusCode::CONFLICT,
                "another launch or content operation is already using this instance",
            )
        })
}

/// List current launcher-managed content without modifying provenance. Missing
/// or manually replaced files are drift and remain recorded in the manifest,
/// but are omitted from this live projection.
pub async fn instance_content(
    state: &AppState,
    instance_id: &str,
) -> Result<InstanceContentResponse, ContentApiError> {
    let game_dir = require_instance_game_dir(state, instance_id)?;
    let _lifecycle_guard = lock_instance_for_content_mutation(state, instance_id).await?;
    let (manifest, live_content) = load_ambient_content_snapshot(game_dir, None)
        .await
        .map_err(content_error_response)?;
    Ok(InstanceContentResponse {
        entries: live_instance_content_entries(&manifest, &live_content),
    })
}

fn live_instance_content_entries(
    manifest: &ContentManifest,
    live_content: &LiveManagedContent,
) -> Vec<InstanceContentEntry> {
    manifest
        .entries()
        .iter()
        .filter_map(|entry| {
            let filename = entry.managed_filename()?;
            live_content.contains(entry).then(|| InstanceContentEntry {
                canonical_id: entry.canonical_id().clone(),
                title: entry.title().map(str::to_string),
                kind: entry.kind(),
                provider: entry.provider(),
                project_id: entry.project_id().to_string(),
                version_id: entry.version_id().to_string(),
                filename: filename.to_string(),
                enabled: entry.enabled(),
            })
        })
        .collect()
}

const UPDATE_CHECK_CONCURRENCY: usize = 6;

/// Which of an instance's tracked entries have a newer compatible version.
/// Best-effort per entry: an item whose provider lookup fails simply reports no
/// update, so one flaky project cannot sink the whole check.
pub async fn instance_content_updates(
    state: &AppState,
    instance_id: &str,
) -> Result<ContentUpdatesResponse, ContentApiError> {
    let target = target::instance_target(state, instance_id).await?;
    let game_dir = target
        .game_dir()
        .map(Path::to_path_buf)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
    let (manifest, live_content) = load_ambient_content_snapshot(game_dir, None)
        .await
        .map_err(content_error_response)?;
    let installed = manifest
        .entries()
        .iter()
        .filter(|entry| live_content.contains(entry))
        .cloned()
        .collect::<Vec<_>>();

    let candidates: Vec<(ManifestEntry, ContentVersion)> = stream::iter(
        installed
            .iter()
            .filter(|entry| entry.kind() != ContentKind::Modpack)
            .cloned()
            .map(|entry| {
                let filter = target.resolution().filter_for(entry.kind());
                async move {
                    let versions = state
                        .content()
                        .versions(entry.canonical_id(), &filter)
                        .await
                        .ok()?;
                    let versions = versions
                        .into_iter()
                        .filter(|version| version_matches_filter(version, &filter))
                        .collect::<Vec<_>>();
                    let latest = newer_version(&versions, entry.version_id())?.clone();
                    Some((entry, latest))
                }
            }),
    )
    .buffer_unordered(UPDATE_CHECK_CONCURRENCY)
    .filter_map(|update| async move { update })
    .collect()
    .await;

    let version_only_dependency_ids: Vec<String> = candidates
        .iter()
        .flat_map(|(_, version)| version.dependencies.iter())
        .filter(|dependency| dependency.project_id.is_none())
        .filter_map(|dependency| dependency.version_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let dependency_versions = if version_only_dependency_ids.is_empty() {
        HashMap::new()
    } else {
        state
            .content()
            .version_identities(&version_only_dependency_ids)
            .await
            .unwrap_or_default()
    };

    let mut updates: Vec<ContentUpdate> = candidates
        .into_iter()
        .filter_map(|(entry, mut latest)| {
            latest.dependencies =
                canonicalize_version_only_dependencies(&latest.dependencies, &dependency_versions);
            if has_unresolved_version_only_incompatibility(&latest.dependencies)
                || version_conflicts_with_installed(&latest, entry.canonical_id(), &installed)
            {
                return None;
            }
            Some(ContentUpdate {
                canonical_id: entry.canonical_id().clone(),
                title: entry.title().map(str::to_string),
                kind: entry.kind(),
                current_version_id: entry.version_id().to_string(),
                latest_version_id: latest.id,
                latest_version_number: latest.version_number,
            })
        })
        .collect();

    updates.sort_by(|a, b| {
        a.title
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .cmp(&b.title.as_deref().unwrap_or("").to_ascii_lowercase())
    });
    Ok(ContentUpdatesResponse { updates })
}

/// Project titles for a batch of ids. Best-effort: a failure costs a nicer label,
/// not the operation. Callers need this because a hash lookup and a version
/// record both name the *version* ("Sodium 0.7.3 for Fabric 1.21.8"), never the
/// project, and the project is what a person calls the thing.
pub(super) async fn project_titles(
    state: &AppState,
    ids: &[CanonicalId],
) -> HashMap<CanonicalId, String> {
    if ids.is_empty() {
        return HashMap::new();
    }
    state.content().titles(ids).await.unwrap_or_default()
}

pub fn content_error_response(error: ContentError) -> ContentApiError {
    tracing::warn!(
        target: "content",
        error_kind = content_error_log_kind(&error),
        "content operation failed"
    );
    let (status, message) = match error {
        ContentError::Unavailable => (
            StatusCode::NOT_FOUND,
            "content is not available for this instance",
        ),
        ContentError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid content request"),
        ContentError::ProviderMetadataInvalid(_) => (
            StatusCode::BAD_GATEWAY,
            "the content provider returned invalid metadata; try again later",
        ),
        ContentError::Status { status, .. } if status.as_u16() == 404 => {
            (StatusCode::NOT_FOUND, "content not found")
        }
        ContentError::Status { .. } | ContentError::Request(_) => (
            StatusCode::BAD_GATEWAY,
            "could not reach the content provider; try again",
        ),
        ContentError::Download(_) => (
            StatusCode::BAD_GATEWAY,
            "a content download failed; check your connection and try again",
        ),
        ContentError::DownloadPreparation(_) => (
            StatusCode::BAD_GATEWAY,
            "could not prepare a content download; check your connection and try again",
        ),
        ContentError::Io(_) | ContentError::Parse(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not complete the content operation",
        ),
    };
    json_error(status, message)
}

fn content_error_log_kind(error: &ContentError) -> &'static str {
    match error {
        ContentError::Request(_) => "provider_request",
        ContentError::Parse(_) => "local_parse",
        ContentError::ProviderMetadataInvalid(_) => "provider_metadata",
        ContentError::Status { .. } => "provider_status",
        ContentError::Io(_) => "file_operation",
        ContentError::Download(_) => "content_download",
        ContentError::DownloadPreparation(_) => "download_preparation",
        ContentError::Unavailable => "content_unavailable",
        ContentError::Invalid(_) => "invalid_request",
    }
}

pub(crate) fn content_execution_error(error: ContentError) -> ContentExecutionError {
    let failure_kind = match &error {
        ContentError::Request(_) | ContentError::DownloadPreparation(_) => {
            Some(ContentExecutionFailureKind::NetworkFailure)
        }
        ContentError::Status { .. } => Some(ContentExecutionFailureKind::ProviderFailure),
        ContentError::ProviderMetadataInvalid(_) => {
            Some(ContentExecutionFailureKind::MetadataInvalid)
        }
        ContentError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Some(ContentExecutionFailureKind::PermissionDenied)
        }
        ContentError::Io(_) => Some(ContentExecutionFailureKind::FileOperation),
        ContentError::Download(_)
        | ContentError::Parse(_)
        | ContentError::Unavailable
        | ContentError::Invalid(_) => None,
    };
    ContentExecutionError {
        response: content_error_response(error),
        failure_kind,
    }
}

fn conflicts_error(conflicts: &[PlanConflict]) -> ContentApiError {
    let detail = conflicts
        .iter()
        .map(|conflict| conflict.detail.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let detail = if detail.chars().count() > MAX_CONFLICT_ERROR_CHARS {
        let retained = MAX_CONFLICT_ERROR_CHARS.saturating_sub(3);
        format!("{}...", detail.chars().take(retained).collect::<String>())
    } else {
        detail
    };
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": detail, "conflicts": conflicts })),
    )
}

pub fn json_error(status: StatusCode, message: &str) -> ContentApiError {
    (status, Json(serde_json::json!({ "error": message })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_content::{ContentService, FileRef};
    use sha2::{Digest, Sha512};
    use std::fs;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_sha512(bytes: &[u8]) -> String {
        hex::encode(Sha512::digest(bytes))
    }

    #[tokio::test]
    async fn p00_b11_contract_cross_owner_direct_service_projects_without_sources() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Application content fixture");
        let address = listener.local_addr().expect("Application fixture address");
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
                    "project_id": "application-project",
                    "slug": "application-project",
                    "title": "Application Project",
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
        let service = ContentService::with_base_url(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("Application fixture client"),
            format!("http://{address}/v2"),
        );
        let mut query = ContentQuery::new(ContentKind::Mod);
        query.limit = 1;

        let page = service.search(&query).await.expect("Core service search");
        let request = server.await.expect("search fixture task");
        let projected = project_search_page(page, &HashSet::new());
        let wire = serde_json::to_value(&projected).expect("serialize Application page");

        assert!(request.starts_with("GET /v2/search?"));
        assert_eq!(
            wire["items"][0]["canonical_id"],
            serde_json::json!("modrinth:application-project")
        );
        assert!(wire["items"][0].get("install_state").is_none());
        assert!(wire["items"][0].get("sources").is_none());
    }

    #[test]
    fn p00_b11_contract_search_install_state_requires_exact_live_bytes() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-search-presence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("mods")).expect("mods");
        let id = CanonicalId::for_project(ProviderId::Modrinth, "tracked-project");
        let file = FileRef {
            url: "https://example.invalid/tracked.jar".to_string(),
            filename: "tracked.jar".to_string(),
            sha1: None,
            sha512: Some(test_sha512(b"tracked")),
            size: Some(b"tracked".len() as u64),
            primary: true,
        };
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(ManifestEntry::managed(
            id.clone(),
            ProviderId::Modrinth,
            "tracked-project".to_string(),
            "tracked-version".to_string(),
            ContentKind::Mod,
            &file,
            Vec::new(),
            None,
        )
        .expect("valid managed entry")).expect("insert managed entry");
        let candidates = HashSet::from([id.clone()]);

        let live_content = project_ambient_live_content(&root, &manifest, Some(&candidates));
        assert!(present_installed_ids(&manifest, &live_content).is_empty());
        fs::write(root.join("mods/tracked.jar"), b"tracked").expect("tracked file");
        let live_content = project_ambient_live_content(&root, &manifest, Some(&candidates));
        assert!(present_installed_ids(&manifest, &live_content).contains(&id));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn content_error_logging_uses_only_a_closed_kind() {
        let error = ContentError::ProviderMetadataInvalid(
            "/private/provider/archive/modrinth.index.json".to_string(),
        );

        assert_eq!(content_error_log_kind(&error), "provider_metadata");
        assert!(!content_error_log_kind(&error).contains('/'));
    }

    #[test]
    fn p00_b11_contract_instance_content_is_an_exact_live_projection() {
        let root = std::env::temp_dir().join(format!(
            "axial-content-live-projection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("mods")).expect("mods");
        let mut manifest = ContentManifest::default();
        let live = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "live"),
            ProviderId::Modrinth,
            "live".to_string(),
            "live-version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/live.jar".to_string(),
                filename: "live.jar".to_string(),
                sha1: None,
                sha512: Some(test_sha512(b"live")),
                size: Some(b"live".len() as u64),
                primary: true,
            },
            Vec::new(),
            Some("Live".to_string()),
        )
        .expect("valid managed entry");
        let missing = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "missing"),
            ProviderId::Modrinth,
            "missing".to_string(),
            "missing-version".to_string(),
            ContentKind::Mod,
            &FileRef {
                url: "https://example.invalid/missing.jar".to_string(),
                filename: "missing.jar".to_string(),
                sha1: None,
                sha512: Some(test_sha512(b"missing")),
                size: Some(b"missing".len() as u64),
                primary: true,
            },
            Vec::new(),
            Some("Missing".to_string()),
        )
        .expect("valid managed entry");
        manifest.try_upsert(live).expect("insert live entry");
        manifest.try_upsert(missing).expect("insert missing entry");
        fs::write(root.join("mods/live.jar"), b"live").expect("live file");
        let before = manifest.clone();

        let live_content = project_ambient_live_content(&root, &manifest, None);
        let entries = live_instance_content_entries(&manifest, &live_content);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].canonical_id.as_str(), "modrinth:live");
        assert_eq!(manifest, before, "projection must not rewrite provenance");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn conflict_error_summary_is_bounded_without_dropping_structured_conflicts() {
        let conflicts = (0..200)
            .map(|_| PlanConflict {
                canonical_id: None,
                kind: ConflictKind::Unavailable,
                detail: "A".repeat(200),
            })
            .collect::<Vec<_>>();

        let (_, body) = conflicts_error(&conflicts);
        let summary = body.0["error"].as_str().expect("conflict summary");

        assert_eq!(summary.chars().count(), MAX_CONFLICT_ERROR_CHARS);
        assert!(summary.ends_with("..."));
        assert_eq!(
            body.0["conflicts"].as_array().map(Vec::len),
            Some(conflicts.len())
        );
    }
}
