//! Content discovery orchestration: search and browse upstream content, resolve
//! a backend-authored install plan against a target, and install verified files
//! into an instance. Provider access, canonicalization, verified download, the
//! provenance manifest and modpack import live in `axial-content`; this module
//! adapts them to the HTTP surface and keeps policy — dependency resolution,
//! conflict detection, compatibility ranking — on the backend.
//!
//! A target is either an instance that exists or a draft one the user is about
//! to create, so browsing before you own anything and adding to a library you
//! already have are the same code path.

pub mod compat;
pub mod pack;
pub mod resolve;
pub mod target;

use crate::state::AppState;
use axial_content::{
    CanonicalContent, CanonicalId, ContentDetail, ContentError, ContentKind, ContentManifest,
    ContentQuery, EntrySource, ManifestEntry, Page, ProviderId, SortOrder, UnmanagedFile,
    install_and_record, reconcile, sha512_file, uninstall,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use compat::{CompatCandidate, CompatDrop};
pub use pack::{ModpackInstallRequest, ModpackInstallResponse, ModpackTarget};
pub use resolve::{ConflictKind, PlanConflict, PlanItem, PlanReason, ResolutionPlan};
pub use target::TargetRef;

use resolve::{require_installable, resolve};
use target::{require_instance_game_dir, resolve_target};

pub type ContentApiError = (StatusCode, Json<serde_json::Value>);

const DEFAULT_SEARCH_LIMIT: u32 = 40;
const MAX_SEARCH_LIMIT: u32 = 100;
const MAX_COMPAT_ITEMS: usize = 40;

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

#[derive(Debug, Clone, Deserialize)]
pub struct ContentSelection {
    pub canonical_id: String,
    pub kind: ContentKind,
    #[serde(default)]
    pub version_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContentPlanRequest {
    pub target: TargetRef,
    pub selections: Vec<ContentSelection>,
}

#[derive(Debug, Deserialize)]
pub struct ContentInstallRequest {
    pub instance_id: String,
    pub selections: Vec<ContentSelection>,
}

#[derive(Debug, Deserialize)]
pub struct ContentCompatRequest {
    pub selections: Vec<ContentSelection>,
}

#[derive(Debug, Serialize)]
pub struct ContentCompatResponse {
    pub candidates: Vec<CompatCandidate>,
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
    pub source: EntrySource,
}

#[derive(Debug, Serialize)]
pub struct InstanceContentResponse {
    pub entries: Vec<InstanceContentEntry>,
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
    // whose manifest cannot be read.
    let manifest = params
        .instance_id
        .as_deref()
        .and_then(|instance_id| require_instance_game_dir(state, instance_id).ok())
        .and_then(|game_dir| ContentManifest::load(&game_dir).ok());

    Ok(Page {
        items: page
            .items
            .into_iter()
            .map(|content| SearchHit {
                install_state: manifest
                    .as_ref()
                    .and_then(|manifest| manifest.find(&content.canonical_id))
                    .map(|_| InstallState::Installed),
                content,
            })
            .collect(),
        offset: page.offset,
        limit: page.limit,
        total: page.total,
    })
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
    require_installable(&request.selections, &target)?;

    // A draft target has nothing installed, so it plans against an empty
    // manifest and every item reads as fresh.
    let manifest = match target.game_dir.as_deref() {
        Some(game_dir) => ContentManifest::load(game_dir).map_err(content_error_response)?,
        None => ContentManifest::default(),
    };
    let resolution = resolve(state, &target, &request.selections, &manifest).await?;

    let instance_id = match &request.target {
        TargetRef::Instance { instance_id } => Some(instance_id.clone()),
        TargetRef::Draft { .. } => None,
    };
    Ok(resolution.into_plan(instance_id, &target))
}

pub async fn content_install(
    state: &AppState,
    request: ContentInstallRequest,
) -> Result<InstanceContentResponse, ContentApiError> {
    let target = target::instance_target(state, &request.instance_id).await?;
    require_installable(&request.selections, &target)?;
    if state
        .sessions()
        .has_active_instance(&request.instance_id)
        .await
    {
        return Err(json_error(
            StatusCode::CONFLICT,
            "cannot change content while the instance is running; stop the game first",
        ));
    }

    let game_dir = target
        .game_dir
        .clone()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
    let manifest = ContentManifest::load(&game_dir).map_err(content_error_response)?;
    let resolution = resolve(state, &target, &request.selections, &manifest).await?;

    let planned = resolution.to_install();
    if !planned.is_empty() {
        install_and_record(state.content().client(), &game_dir, &planned, |_| {})
            .await
            .map_err(content_error_response)?;
    }

    instance_content(state, &request.instance_id).await
}

/// Which instances a staged set of content could live in. Drives the flow where
/// someone picks content before they have anywhere to put it.
pub async fn content_compatibility(
    state: &AppState,
    request: ContentCompatRequest,
) -> Result<ContentCompatResponse, ContentApiError> {
    if request.selections.is_empty() {
        return Ok(ContentCompatResponse {
            candidates: Vec::new(),
        });
    }
    if request.selections.len() > MAX_COMPAT_ITEMS {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "too many items selected at once",
        ));
    }

    let mut items = Vec::with_capacity(request.selections.len());
    for selection in &request.selections {
        let id = CanonicalId(selection.canonical_id.clone());
        let detail = state
            .content()
            .detail(&id)
            .await
            .map_err(content_error_response)?;
        items.push(compat::CompatItem {
            canonical_id: id,
            title: detail.content.title,
            kind: detail.content.kind,
            loaders: detail.content.loaders,
            game_versions: detail.content.game_versions,
        });
    }

    Ok(ContentCompatResponse {
        candidates: compat::rank_candidates(&items),
    })
}

pub async fn content_uninstall(
    state: &AppState,
    instance_id: &str,
    canonical_id: &str,
) -> Result<InstanceContentResponse, ContentApiError> {
    let game_dir = require_instance_game_dir(state, instance_id)?;
    if state.sessions().has_active_instance(instance_id).await {
        return Err(json_error(
            StatusCode::CONFLICT,
            "cannot change content while the instance is running; stop the game first",
        ));
    }
    uninstall(&game_dir, &CanonicalId(canonical_id.to_string())).map_err(content_error_response)?;
    instance_content(state, instance_id).await
}

/// List an instance's tracked content. Along the way it reconciles the manifest
/// against disk (dropping vanished files) and retrofits unmanaged files by
/// hashing them and identifying them upstream.
pub async fn instance_content(
    state: &AppState,
    instance_id: &str,
) -> Result<InstanceContentResponse, ContentApiError> {
    let game_dir = require_instance_game_dir(state, instance_id)?;
    let mut manifest = ContentManifest::load(&game_dir).map_err(content_error_response)?;
    let report = reconcile(&game_dir, &manifest);

    let mut changed = false;
    for missing in &report.missing {
        if manifest.remove(missing).is_some() {
            changed = true;
        }
    }

    if retrofit_unmanaged(state, &report.unmanaged, &mut manifest).await {
        changed = true;
    }

    if changed {
        manifest.save(&game_dir).map_err(content_error_response)?;
    }

    let entries = manifest
        .entries
        .iter()
        .map(|entry| InstanceContentEntry {
            canonical_id: entry.canonical_id.clone(),
            title: entry.title.clone(),
            kind: entry.kind,
            provider: entry.provider,
            project_id: entry.project_id.clone(),
            version_id: entry.version_id.clone(),
            filename: entry.filename.clone(),
            enabled: entry.enabled,
            source: entry.source,
        })
        .collect();
    Ok(InstanceContentResponse { entries })
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

/// Hash whatever is sitting in the content directories that we did not put there,
/// ask the provider what it is, and adopt what it recognizes. This is how an
/// instance that predates Discover gains provenance. Every managed kind is
/// covered, not just mods.
async fn retrofit_unmanaged(
    state: &AppState,
    unmanaged: &[UnmanagedFile],
    manifest: &mut ContentManifest,
) -> bool {
    if unmanaged.is_empty() {
        return false;
    }

    let files = unmanaged.to_vec();
    let hashed = tokio::task::spawn_blocking(move || {
        let mut hashes: Vec<(String, UnmanagedFile)> = Vec::new();
        for file in files {
            if let Ok(hash) = sha512_file(&file.path) {
                hashes.push((hash, file));
            }
        }
        hashes
    })
    .await
    .unwrap_or_default();

    if hashed.is_empty() {
        return false;
    }

    let by_hash: HashMap<String, UnmanagedFile> = hashed.into_iter().collect();
    let hashes: Vec<String> = by_hash.keys().cloned().collect();
    let Ok(identified) = state.content().identify(&hashes).await else {
        return false;
    };

    let ids: Vec<CanonicalId> = identified
        .values()
        .map(|identity| CanonicalId::for_project(identity.provider, &identity.project_id))
        .collect();
    let titles = project_titles(state, &ids).await;

    let mut changed = false;
    for (hash, mut identity) in identified {
        if let Some(file) = by_hash.get(&hash) {
            let id = CanonicalId::for_project(identity.provider, &identity.project_id);
            if let Some(title) = titles.get(&id) {
                identity.title = Some(title.clone());
            }
            manifest.upsert(ManifestEntry::imported(
                file.kind,
                file.filename.clone(),
                identity,
            ));
            changed = true;
        }
    }
    changed
}

pub fn content_error_response(error: ContentError) -> ContentApiError {
    tracing::warn!(target: "content", error = %error, "content operation failed");
    let (status, message) = match error {
        ContentError::Unavailable => (
            StatusCode::NOT_FOUND,
            "content is not available for this instance",
        ),
        ContentError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid content request"),
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
        ContentError::Io(_) | ContentError::Parse(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not complete the content operation",
        ),
    };
    json_error(status, message)
}

pub fn json_error(status: StatusCode, message: &str) -> ContentApiError {
    (status, Json(serde_json::json!({ "error": message })))
}
