//! Content discovery orchestration: search and browse upstream content, resolve
//! a backend-authored install plan against a target instance, and install
//! verified files into it. Provider access, canonicalization, verified download,
//! and the provenance manifest live in `axial-content`; this module adapts them
//! to the HTTP surface and keeps policy (dependency and conflict resolution) on
//! the backend.

use crate::state::AppState;
use axial_content::{
    CanonicalContent, CanonicalId, ContentDependency, ContentDetail, ContentError, ContentKind,
    ContentManifest, ContentQuery, ContentVersion, DependencyKind, EntrySource, FileRef,
    LoaderGameFilter, ManifestEntry, Page, PlannedFile, ProviderId, SortOrder, UnmanagedFile,
    install_and_record, reconcile, sha512_file, uninstall,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

pub type ContentApiError = (StatusCode, Json<serde_json::Value>);

const DEFAULT_SEARCH_LIMIT: u32 = 40;
const MAX_SEARCH_LIMIT: u32 = 100;
const MAX_RESOLVE_ITEMS: usize = 200;

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
}

#[derive(Debug, Deserialize)]
pub struct ContentSelection {
    pub canonical_id: String,
    pub kind: ContentKind,
    #[serde(default)]
    pub version_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContentPlanRequest {
    pub instance_id: String,
    pub selections: Vec<ContentSelection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReason {
    Selected,
    Dependency,
}

#[derive(Debug, Serialize)]
pub struct PlanItem {
    pub canonical_id: CanonicalId,
    pub title: String,
    pub kind: ContentKind,
    pub project_id: String,
    pub version_id: String,
    pub version_number: String,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub reason: PlanReason,
    pub already_installed: bool,
    pub update: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    Unavailable,
    Incompatible,
}

#[derive(Debug, Serialize)]
pub struct PlanConflict {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<CanonicalId>,
    pub kind: ConflictKind,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct ResolutionPlan {
    pub instance_id: String,
    pub loader: String,
    pub game_version: String,
    pub items: Vec<PlanItem>,
    pub conflicts: Vec<PlanConflict>,
    pub total_download_bytes: u64,
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
) -> Result<Page<CanonicalContent>, ContentApiError> {
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

    state
        .content()
        .search(&query)
        .await
        .map_err(content_error_response)
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
    let target = instance_target(state, &request.instance_id).await?;
    require_installable(&request.selections, &target)?;
    let manifest = ContentManifest::load(&target.game_dir).map_err(content_error_response)?;
    let resolution = resolve(state, &target, &request.selections, &manifest).await?;
    Ok(resolution.into_plan(request.instance_id, target.loader, target.game_version))
}

pub async fn content_install(
    state: &AppState,
    request: ContentPlanRequest,
) -> Result<InstanceContentResponse, ContentApiError> {
    let target = instance_target(state, &request.instance_id).await?;
    require_installable(&request.selections, &target)?;
    if state
        .sessions()
        .has_active_instance(&request.instance_id)
        .await
    {
        return Err(json_error(
            StatusCode::CONFLICT,
            "cannot change mods while the instance is running; stop the game first",
        ));
    }

    let manifest = ContentManifest::load(&target.game_dir).map_err(content_error_response)?;
    let resolution = resolve(state, &target, &request.selections, &manifest).await?;

    let planned: Vec<PlannedFile> = resolution
        .items
        .iter()
        .filter(|item| !item.already_installed || item.update)
        .map(ResolvedItem::to_planned)
        .collect();

    if !planned.is_empty() {
        install_and_record(state.content().client(), &target.game_dir, &planned, |_| {})
            .await
            .map_err(content_error_response)?;
    }

    instance_content(state, &request.instance_id).await
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
            "cannot change mods while the instance is running; stop the game first",
        ));
    }
    uninstall(&game_dir, &CanonicalId(canonical_id.to_string())).map_err(content_error_response)?;
    instance_content(state, instance_id).await
}

/// List an instance's tracked content. Along the way it reconciles the manifest
/// against disk (dropping vanished files) and retrofits unmanaged jars by hashing
/// them and identifying them upstream.
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

struct InstanceTarget {
    game_dir: PathBuf,
    loader: String,
    game_version: String,
    supports_mods: bool,
}

struct ResolvedItem {
    canonical_id: CanonicalId,
    provider: ProviderId,
    project_id: String,
    kind: ContentKind,
    version_id: String,
    version_number: String,
    title: String,
    file: FileRef,
    dependencies: Vec<ContentDependency>,
    reason: PlanReason,
    already_installed: bool,
    update: bool,
}

impl ResolvedItem {
    fn to_planned(&self) -> PlannedFile {
        PlannedFile {
            canonical_id: self.canonical_id.clone(),
            provider: self.provider,
            project_id: self.project_id.clone(),
            version_id: self.version_id.clone(),
            kind: self.kind,
            file: self.file.clone(),
            dependencies: self.dependencies.clone(),
            title: Some(self.title.clone()),
        }
    }
}

struct Resolution {
    items: Vec<ResolvedItem>,
    conflicts: Vec<PlanConflict>,
}

impl Resolution {
    fn into_plan(
        self,
        instance_id: String,
        loader: String,
        game_version: String,
    ) -> ResolutionPlan {
        let total_download_bytes = self
            .items
            .iter()
            .filter(|item| !item.already_installed || item.update)
            .filter_map(|item| item.file.size)
            .sum();
        let items = self
            .items
            .into_iter()
            .map(|item| PlanItem {
                canonical_id: item.canonical_id,
                title: item.title,
                kind: item.kind,
                project_id: item.project_id,
                version_id: item.version_id,
                version_number: item.version_number,
                filename: item.file.filename,
                size: item.file.size,
                reason: item.reason,
                already_installed: item.already_installed,
                update: item.update,
            })
            .collect();
        ResolutionPlan {
            instance_id,
            loader,
            game_version,
            items,
            conflicts: self.conflicts,
            total_download_bytes,
        }
    }
}

async fn resolve(
    state: &AppState,
    target: &InstanceTarget,
    selections: &[ContentSelection],
    manifest: &ContentManifest,
) -> Result<Resolution, ContentApiError> {
    let filter = LoaderGameFilter {
        loader: Some(target.loader.clone()),
        game_version: Some(target.game_version.clone()),
    };
    let mut resolved_ids: HashSet<CanonicalId> = HashSet::new();
    let mut items: Vec<ResolvedItem> = Vec::new();
    let mut conflicts: Vec<PlanConflict> = Vec::new();

    let mut queue: VecDeque<(CanonicalId, Option<String>, PlanReason)> = selections
        .iter()
        .map(|selection| {
            (
                CanonicalId(selection.canonical_id.clone()),
                selection.version_id.clone(),
                PlanReason::Selected,
            )
        })
        .collect();

    while let Some((canonical_id, forced_version, reason)) = queue.pop_front() {
        if !resolved_ids.insert(canonical_id.clone()) {
            continue;
        }
        if items.len() >= MAX_RESOLVE_ITEMS {
            break;
        }

        let versions = match state.content().versions(&canonical_id, &filter).await {
            Ok(versions) => versions,
            Err(ContentError::Status { status, .. }) if status.as_u16() == 404 => {
                conflicts.push(unavailable_conflict(&canonical_id));
                continue;
            }
            Err(error) => return Err(content_error_response(error)),
        };

        let Some(version) = pick_version(&versions, forced_version.as_deref()) else {
            conflicts.push(unavailable_conflict(&canonical_id));
            continue;
        };
        let Some(file) = version.primary_file().cloned() else {
            conflicts.push(unavailable_conflict(&canonical_id));
            continue;
        };

        for dependency in &version.dependencies {
            match dependency.kind {
                DependencyKind::Required => {
                    if let Some(project_id) = &dependency.project_id {
                        queue.push_back((
                            CanonicalId::for_project(ProviderId::Modrinth, project_id),
                            dependency.version_id.clone(),
                            PlanReason::Dependency,
                        ));
                    }
                }
                DependencyKind::Incompatible => {
                    if let Some(project_id) = &dependency.project_id {
                        let incompatible =
                            CanonicalId::for_project(ProviderId::Modrinth, project_id);
                        if manifest.find(&incompatible).is_some() {
                            conflicts.push(incompatible_conflict(&incompatible));
                        }
                    }
                }
                DependencyKind::Optional | DependencyKind::Embedded => {}
            }
        }

        let existing = manifest.find(&canonical_id);
        let already_installed = existing.is_some();
        let update = existing.is_some_and(|entry| entry.version_id != version.id);
        let project_id = canonical_id.project_id().to_string();

        items.push(ResolvedItem {
            canonical_id,
            provider: ProviderId::Modrinth,
            project_id,
            kind: ContentKind::Mod,
            version_id: version.id.clone(),
            version_number: version.version_number.clone(),
            title: version.name.clone(),
            file,
            dependencies: version.dependencies.clone(),
            reason,
            already_installed,
            update,
        });
    }

    Ok(Resolution { items, conflicts })
}

fn pick_version<'a>(
    versions: &'a [ContentVersion],
    forced: Option<&str>,
) -> Option<&'a ContentVersion> {
    if let Some(forced) = forced {
        return versions.iter().find(|version| version.id == forced);
    }
    versions
        .iter()
        .find(|version| {
            matches!(version.channel, axial_content::ReleaseChannel::Release)
                && version.primary_file().is_some()
        })
        .or_else(|| {
            versions
                .iter()
                .find(|version| version.primary_file().is_some())
        })
}

fn require_installable(
    selections: &[ContentSelection],
    target: &InstanceTarget,
) -> Result<(), ContentApiError> {
    if selections.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "no content selected"));
    }
    if selections
        .iter()
        .any(|selection| selection.kind != ContentKind::Mod)
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "only mods can be installed right now",
        ));
    }
    if !target.supports_mods {
        return Err(json_error(
            StatusCode::PRECONDITION_FAILED,
            "this instance has no mod loader; add mods to a modded instance",
        ));
    }
    Ok(())
}

async fn retrofit_unmanaged(
    state: &AppState,
    unmanaged: &[UnmanagedFile],
    manifest: &mut ContentManifest,
) -> bool {
    let mod_files: Vec<UnmanagedFile> = unmanaged
        .iter()
        .filter(|file| file.kind == ContentKind::Mod)
        .cloned()
        .collect();
    if mod_files.is_empty() {
        return false;
    }

    let hashed = tokio::task::spawn_blocking(move || {
        let mut hashes: Vec<(String, UnmanagedFile)> = Vec::new();
        for file in mod_files {
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

    let mut changed = false;
    for (hash, identity) in identified {
        if let Some(file) = by_hash.get(&hash) {
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

async fn instance_target(
    state: &AppState,
    instance_id: &str,
) -> Result<InstanceTarget, ContentApiError> {
    let instance = state
        .instances()
        .get(instance_id)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
    let versions = crate::application::version::installed_versions(state)
        .await?
        .versions;
    let display = state
        .instances()
        .enrich(&versions)
        .into_iter()
        .find(|entry| entry.instance.id == instance.id)
        .map(|entry| entry.version_display)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;

    Ok(InstanceTarget {
        game_dir: state.instances().game_dir(&instance.id),
        loader: display.loader_key,
        game_version: display.minecraft_label,
        supports_mods: display.supports_mods,
    })
}

fn require_instance_game_dir(
    state: &AppState,
    instance_id: &str,
) -> Result<PathBuf, ContentApiError> {
    let instance = state
        .instances()
        .get(instance_id)
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "instance not found"))?;
    Ok(state.instances().game_dir(&instance.id))
}

fn unavailable_conflict(canonical_id: &CanonicalId) -> PlanConflict {
    PlanConflict {
        canonical_id: Some(canonical_id.clone()),
        kind: ConflictKind::Unavailable,
        detail: "no compatible version for this instance's loader and Minecraft version"
            .to_string(),
    }
}

fn incompatible_conflict(canonical_id: &CanonicalId) -> PlanConflict {
    PlanConflict {
        canonical_id: Some(canonical_id.clone()),
        kind: ConflictKind::Incompatible,
        detail: "conflicts with content already installed in this instance".to_string(),
    }
}

fn content_error_response(error: ContentError) -> ContentApiError {
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

fn json_error(status: StatusCode, message: &str) -> ContentApiError {
    (status, Json(serde_json::json!({ "error": message })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_content::ReleaseChannel;

    fn file(name: &str, size: Option<u64>) -> FileRef {
        FileRef {
            url: format!("https://example.invalid/{name}"),
            filename: name.to_string(),
            sha1: Some("a".repeat(40)),
            sha512: None,
            size,
            primary: true,
        }
    }

    fn version(id: &str, channel: ReleaseChannel, files: Vec<FileRef>) -> ContentVersion {
        ContentVersion {
            id: id.to_string(),
            name: format!("Mod {id}"),
            version_number: id.to_string(),
            game_versions: vec!["1.21.6".to_string()],
            loaders: vec!["fabric".to_string()],
            channel,
            published: None,
            downloads: 0,
            files,
            dependencies: Vec::new(),
        }
    }

    fn mod_selection(id: &str) -> ContentSelection {
        ContentSelection {
            canonical_id: id.to_string(),
            kind: ContentKind::Mod,
            version_id: None,
        }
    }

    fn modded_target() -> InstanceTarget {
        InstanceTarget {
            game_dir: PathBuf::from("/tmp/does-not-matter"),
            loader: "fabric".to_string(),
            game_version: "1.21.6".to_string(),
            supports_mods: true,
        }
    }

    #[test]
    fn pick_version_prefers_release_with_a_file() {
        let versions = vec![
            version("beta-1", ReleaseChannel::Beta, vec![file("beta.jar", None)]),
            version(
                "rel-1",
                ReleaseChannel::Release,
                vec![file("rel.jar", None)],
            ),
        ];
        assert_eq!(pick_version(&versions, None).unwrap().id, "rel-1");
    }

    #[test]
    fn pick_version_honors_a_forced_id() {
        let versions = vec![
            version(
                "rel-1",
                ReleaseChannel::Release,
                vec![file("rel.jar", None)],
            ),
            version("beta-1", ReleaseChannel::Beta, vec![file("beta.jar", None)]),
        ];
        assert_eq!(
            pick_version(&versions, Some("beta-1")).unwrap().id,
            "beta-1"
        );
        assert!(pick_version(&versions, Some("missing")).is_none());
    }

    #[test]
    fn pick_version_falls_back_to_prerelease_when_no_release_has_a_file() {
        let versions = vec![
            version("rel-1", ReleaseChannel::Release, vec![]),
            version("beta-1", ReleaseChannel::Beta, vec![file("beta.jar", None)]),
        ];
        assert_eq!(pick_version(&versions, None).unwrap().id, "beta-1");
    }

    #[test]
    fn require_installable_rejects_empty_non_mod_and_loaderless() {
        assert!(require_installable(&[], &modded_target()).is_err());

        let shader = ContentSelection {
            canonical_id: "modrinth:x".to_string(),
            kind: ContentKind::ShaderPack,
            version_id: None,
        };
        assert!(require_installable(std::slice::from_ref(&shader), &modded_target()).is_err());

        let vanilla = InstanceTarget {
            supports_mods: false,
            ..modded_target()
        };
        assert!(require_installable(&[mod_selection("modrinth:x")], &vanilla).is_err());

        assert!(require_installable(&[mod_selection("modrinth:x")], &modded_target()).is_ok());
    }

    #[test]
    fn plan_bytes_count_only_files_to_install() {
        let resolved = |id: &str, size: u64, already: bool, update: bool| ResolvedItem {
            canonical_id: CanonicalId(format!("modrinth:{id}")),
            provider: ProviderId::Modrinth,
            project_id: id.to_string(),
            kind: ContentKind::Mod,
            version_id: format!("{id}-v"),
            version_number: "1".to_string(),
            title: id.to_string(),
            file: file(&format!("{id}.jar"), Some(size)),
            dependencies: Vec::new(),
            reason: PlanReason::Selected,
            already_installed: already,
            update,
        };
        let resolution = Resolution {
            items: vec![
                resolved("fresh", 100, false, false),
                resolved("update", 200, true, true),
                resolved("skip", 400, true, false),
            ],
            conflicts: Vec::new(),
        };
        let plan = resolution.into_plan(
            "inst".to_string(),
            "fabric".to_string(),
            "1.21.6".to_string(),
        );
        assert_eq!(plan.total_download_bytes, 300);
        assert_eq!(plan.items.len(), 3);
    }
}
