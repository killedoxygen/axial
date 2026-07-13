//! Backend-authored resolution: expand required dependencies, flag conflicts
//! against what the target already has, and pick a concrete version and file for
//! every item. The client never chooses a download URL; it only echoes back
//! selections, and this pass re-derives everything server-side.

use super::target::ResolveTarget;
use super::{ContentApiError, ContentSelection, content_error_response, json_error};
use axial_content::{
    CanonicalId, ContentDependency, ContentError, ContentKind, ContentManifest, ContentVersion,
    DependencyKind, FileRef, ManifestEntry, PlannedFile, ProjectMetadata, ProviderId,
    ReleaseChannel, entry_file_present,
};
use axum::http::StatusCode;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::state::AppState;

const MAX_RESOLVE_ITEMS: usize = 200;

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
    pub sha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha512: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<ContentDependency>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub loader: String,
    pub game_version: String,
    pub items: Vec<PlanItem>,
    pub conflicts: Vec<PlanConflict>,
    pub total_download_bytes: u64,
}

pub struct ResolvedItem {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub kind: ContentKind,
    pub version_id: String,
    pub version_number: String,
    pub title: String,
    pub file: FileRef,
    pub dependencies: Vec<ContentDependency>,
    pub reason: PlanReason,
    pub already_installed: bool,
    pub update: bool,
}

impl ResolvedItem {
    pub fn to_planned(&self) -> PlannedFile {
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

pub struct Resolution {
    pub items: Vec<ResolvedItem>,
    pub conflicts: Vec<PlanConflict>,
}

impl Resolution {
    /// Files that actually need downloading: what is already installed at the
    /// resolved version is left alone.
    pub fn to_install(&self) -> Vec<PlannedFile> {
        self.items
            .iter()
            .filter(|item| !item.already_installed || item.update)
            .map(ResolvedItem::to_planned)
            .collect()
    }

    pub fn into_plan(self, instance_id: Option<String>, target: &ResolveTarget) -> ResolutionPlan {
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
                sha1: item.file.sha1,
                sha512: item.file.sha512,
                size: item.file.size,
                dependencies: item.dependencies,
                reason: item.reason,
                already_installed: item.already_installed,
                update: item.update,
            })
            .collect();
        ResolutionPlan {
            instance_id,
            loader: target.loader.clone(),
            game_version: target.game_version.clone(),
            items,
            conflicts: self.conflicts,
            total_download_bytes,
        }
    }
}

/// Reject a provider-authored type the target cannot physically accept.
fn require_installable_kind(
    kind: ContentKind,
    target: &ResolveTarget,
) -> Result<(), ContentApiError> {
    if kind == ContentKind::Modpack {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "a modpack is installed as its own instance, not added to one",
        ));
    }
    if !target.supports_mods && kind.requires_mod_loader() {
        return Err(json_error(
            StatusCode::PRECONDITION_FAILED,
            "this instance has no mod loader; add mods to a modded instance",
        ));
    }
    Ok(())
}

fn validate_selected_kinds(
    selections: &[ContentSelection],
    metadata: &HashMap<CanonicalId, ProjectMetadata>,
    target: &ResolveTarget,
) -> Result<(), ContentApiError> {
    if selections.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "no content selected"));
    }
    for selection in selections {
        let canonical_id = CanonicalId(selection.canonical_id.clone());
        let Some(authoritative) = metadata.get(&canonical_id) else {
            continue;
        };
        if selection.kind != authoritative.kind {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "the selected content type changed; refresh it and try again",
            ));
        }
        require_installable_kind(authoritative.kind, target)?;
    }
    Ok(())
}

struct ResolvePass {
    resolution: Resolution,
    retry_with_exact: Option<(CanonicalId, String)>,
}

pub async fn resolve(
    state: &AppState,
    target: &ResolveTarget,
    selections: &[ContentSelection],
    manifest: &ContentManifest,
) -> Result<Resolution, ContentApiError> {
    if selections.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "no content selected"));
    }
    let mut exact_requirements = HashMap::new();
    for selection in selections {
        let Some(version_id) = selection.version_id.as_ref() else {
            continue;
        };
        let canonical_id = CanonicalId(selection.canonical_id.clone());
        if let Some(existing) = exact_requirements.insert(canonical_id.clone(), version_id.clone())
            && existing != *version_id
        {
            return Ok(Resolution {
                items: Vec::new(),
                conflicts: vec![exact_dependency_conflict(
                    &canonical_id,
                    &existing,
                    version_id,
                )],
            });
        }
    }

    for _ in 0..MAX_RESOLVE_ITEMS {
        let pass = resolve_pass(state, target, selections, manifest, &exact_requirements).await?;
        let Some((canonical_id, required_version)) = pass.retry_with_exact else {
            return Ok(pass.resolution);
        };
        exact_requirements.insert(canonical_id, required_version);
    }

    Ok(Resolution {
        items: Vec::new(),
        conflicts: vec![PlanConflict {
            canonical_id: None,
            kind: ConflictKind::Unavailable,
            detail: "could not stabilize exact dependency requirements".to_string(),
        }],
    })
}

async fn resolve_pass(
    state: &AppState,
    target: &ResolveTarget,
    selections: &[ContentSelection],
    manifest: &ContentManifest,
    exact_requirements: &HashMap<CanonicalId, String>,
) -> Result<ResolvePass, ContentApiError> {
    let selected_ids: Vec<CanonicalId> = selections
        .iter()
        .map(|selection| CanonicalId(selection.canonical_id.clone()))
        .collect();
    let mut metadata = state
        .content()
        .metadata(&selected_ids)
        .await
        .map_err(content_error_response)?;
    validate_selected_kinds(selections, &metadata, target)?;
    let mut metadata_requested: HashSet<CanonicalId> = selected_ids.into_iter().collect();
    let mut resolved_versions: HashMap<CanonicalId, String> = HashMap::new();
    let mut items: Vec<ResolvedItem> = Vec::new();
    let mut conflicts: Vec<PlanConflict> = Vec::new();
    let mut incompatibilities: HashSet<(CanonicalId, CanonicalId)> = HashSet::new();

    let mut queue: VecDeque<(CanonicalId, Option<String>, PlanReason)> = selections
        .iter()
        .map(|selection| {
            let canonical_id = CanonicalId(selection.canonical_id.clone());
            let version_id = selection
                .version_id
                .clone()
                .or_else(|| exact_requirements.get(&canonical_id).cloned());
            (canonical_id, version_id, PlanReason::Selected)
        })
        .collect();

    while let Some((canonical_id, forced_version, reason)) = queue.pop_front() {
        if let Some(chosen_version) = resolved_versions.get(&canonical_id) {
            if let Some(required_version) = forced_version.as_deref()
                && required_version != chosen_version
            {
                if exact_requirement_needs_retry(exact_requirements, &canonical_id) {
                    return Ok(ResolvePass {
                        resolution: Resolution { items, conflicts },
                        retry_with_exact: Some((canonical_id, required_version.to_string())),
                    });
                }
                conflicts.push(exact_dependency_conflict(
                    &canonical_id,
                    chosen_version,
                    required_version,
                ));
            }
            continue;
        }
        if items.len() >= MAX_RESOLVE_ITEMS {
            conflicts.push(PlanConflict {
                canonical_id: Some(canonical_id),
                kind: ConflictKind::Unavailable,
                detail: "could not be resolved because the dependency graph is too large"
                    .to_string(),
            });
            break;
        }

        if !metadata_requested.contains(&canonical_id) {
            let mut missing_ids = HashSet::new();
            missing_ids.insert(canonical_id.clone());
            missing_ids.extend(queue.iter().filter_map(|(queued_id, _, _)| {
                (!metadata_requested.contains(queued_id)).then(|| queued_id.clone())
            }));
            let missing_ids: Vec<CanonicalId> = missing_ids.into_iter().collect();
            let fetched = state
                .content()
                .metadata(&missing_ids)
                .await
                .map_err(content_error_response)?;
            metadata.extend(fetched);
            metadata_requested.extend(missing_ids);
        }
        let Some(project) = metadata.get(&canonical_id).cloned() else {
            conflicts.push(unavailable_conflict(&canonical_id));
            continue;
        };
        let kind = project.kind;
        require_installable_kind(kind, target)?;

        let filter = target.filter_for(kind);
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
        resolved_versions.insert(canonical_id.clone(), version.id.clone());

        for dependency in &version.dependencies {
            match dependency.kind {
                DependencyKind::Required => {
                    if let Some(project_id) = &dependency.project_id {
                        queue.push_back((
                            CanonicalId::for_project(ProviderId::Modrinth, project_id),
                            dependency.version_id.clone(),
                            PlanReason::Dependency,
                        ));
                    } else {
                        conflicts.push(PlanConflict {
                            canonical_id: Some(canonical_id.clone()),
                            kind: ConflictKind::Unavailable,
                            detail: "has a required dependency that could not be identified"
                                .to_string(),
                        });
                    }
                }
                DependencyKind::Incompatible => {
                    if let Some(project_id) = &dependency.project_id {
                        let incompatible =
                            CanonicalId::for_project(ProviderId::Modrinth, project_id);
                        if let Some(entry) = manifest.find(&incompatible)
                            && incompatibilities
                                .insert((canonical_id.clone(), entry.canonical_id.clone()))
                        {
                            conflicts.push(incompatible_conflict(&canonical_id, entry));
                        }
                    }
                }
                DependencyKind::Optional | DependencyKind::Embedded => {}
            }
        }

        for entry in installed_entries_incompatible_with(manifest, &canonical_id) {
            if incompatibilities.insert((canonical_id.clone(), entry.canonical_id.clone())) {
                conflicts.push(incompatible_conflict(&canonical_id, entry));
            }
        }

        let existing = manifest.find(&canonical_id);
        let (already_installed, update) =
            resolved_install_state(existing, target.game_dir.as_deref(), &version.id);
        let project_id = canonical_id.project_id().to_string();

        items.push(ResolvedItem {
            canonical_id,
            provider: ProviderId::Modrinth,
            project_id,
            kind,
            version_id: version.id.clone(),
            version_number: version.version_number.clone(),
            title: if project.title.trim().is_empty() {
                version.name.clone()
            } else {
                project.title
            },
            file,
            dependencies: version.dependencies.clone(),
            reason,
            already_installed,
            update,
        });
    }

    conflicts.extend(selected_incompatibility_conflicts(&items));

    apply_metadata_titles(&metadata, &mut conflicts);
    Ok(ResolvePass {
        resolution: Resolution { items, conflicts },
        retry_with_exact: None,
    })
}

fn exact_requirement_needs_retry(
    exact_requirements: &HashMap<CanonicalId, String>,
    canonical_id: &CanonicalId,
) -> bool {
    !exact_requirements.contains_key(canonical_id)
}

fn resolved_install_state(
    existing: Option<&ManifestEntry>,
    game_dir: Option<&std::path::Path>,
    resolved_version_id: &str,
) -> (bool, bool) {
    let already_installed =
        existing.is_some_and(|entry| game_dir.is_none_or(|root| entry_file_present(root, entry)));
    let update =
        already_installed && existing.is_some_and(|entry| entry.version_id != resolved_version_id);
    (already_installed, update)
}

fn selected_incompatibility_conflicts(items: &[ResolvedItem]) -> Vec<PlanConflict> {
    let resolved_projects: HashSet<String> =
        items.iter().map(|item| item.project_id.clone()).collect();
    let mut selected_incompatibilities = HashSet::new();
    let mut conflicts = Vec::new();
    for item in items {
        for dependency in &item.dependencies {
            if dependency.kind != DependencyKind::Incompatible {
                continue;
            }
            let Some(project_id) = dependency.project_id.as_deref() else {
                continue;
            };
            if !resolved_projects.contains(project_id) {
                continue;
            }
            let key = (item.project_id.clone(), project_id.to_string());
            if selected_incompatibilities.insert(key) {
                conflicts.push(PlanConflict {
                    canonical_id: Some(item.canonical_id.clone()),
                    kind: ConflictKind::Incompatible,
                    detail: format!("is incompatible with selected project {project_id}"),
                });
            }
        }
    }
    conflicts
}

/// Give every conflict a provider-authored subject so "has no compatible
/// version" becomes "Sodium has no compatible version".
fn apply_metadata_titles(
    metadata: &HashMap<CanonicalId, ProjectMetadata>,
    conflicts: &mut [PlanConflict],
) {
    for conflict in conflicts {
        if let Some(id) = &conflict.canonical_id {
            let label = metadata
                .get(id)
                .map(|project| project.title.clone())
                .unwrap_or_else(|| id.project_id().to_string());
            conflict.detail = format!("{label} {}", conflict.detail);
        }
    }
}

/// The version an installed entry should move to, if any. Versions arrive
/// newest-first from the provider, so "newer" is "earlier in the list"; an
/// installed version that no longer appears (a loader or Minecraft change
/// filtered it out) always yields to the best compatible pick.
pub fn newer_version<'a>(
    versions: &'a [ContentVersion],
    current_version_id: &str,
) -> Option<&'a ContentVersion> {
    let picked = pick_version(versions, None)?;
    if picked.id == current_version_id {
        return None;
    }
    let picked_index = versions
        .iter()
        .position(|version| version.id == picked.id)?;
    match versions
        .iter()
        .position(|version| version.id == current_version_id)
    {
        Some(current_index) => (picked_index < current_index).then_some(picked),
        None => Some(picked),
    }
}

/// Whether moving to this version would introduce a declared incompatibility
/// with something the instance already has. The entry being updated is exempt:
/// it is being replaced, not conflicted with.
pub fn version_conflicts_with_installed(
    version: &ContentVersion,
    own_id: &CanonicalId,
    installed: &[ManifestEntry],
) -> bool {
    let candidate_declares_conflict = version.dependencies.iter().any(|dependency| {
        dependency.kind == DependencyKind::Incompatible
            && dependency.project_id.as_deref().is_some_and(|project_id| {
                let id = CanonicalId::for_project(ProviderId::Modrinth, project_id);
                id != *own_id && installed.iter().any(|entry| entry.canonical_id == id)
            })
    });
    if candidate_declares_conflict {
        return true;
    }

    installed.iter().any(|entry| {
        entry.canonical_id != *own_id
            && entry.dependencies.iter().any(|dependency| {
                dependency.kind == DependencyKind::Incompatible
                    && dependency
                        .project_id
                        .as_deref()
                        .is_some_and(|project_id| project_id == own_id.project_id())
            })
    })
}

pub fn pick_version<'a>(
    versions: &'a [ContentVersion],
    forced: Option<&str>,
) -> Option<&'a ContentVersion> {
    if let Some(forced) = forced {
        return versions.iter().find(|version| version.id == forced);
    }
    versions
        .iter()
        .find(|version| {
            matches!(version.channel, ReleaseChannel::Release) && version.primary_file().is_some()
        })
        .or_else(|| {
            versions
                .iter()
                .find(|version| version.primary_file().is_some())
        })
}

fn unavailable_conflict(canonical_id: &CanonicalId) -> PlanConflict {
    PlanConflict {
        canonical_id: Some(canonical_id.clone()),
        kind: ConflictKind::Unavailable,
        detail: "has no compatible version for this loader and Minecraft version".to_string(),
    }
}

fn exact_dependency_conflict(
    canonical_id: &CanonicalId,
    chosen_version: &str,
    required_version: &str,
) -> PlanConflict {
    PlanConflict {
        canonical_id: Some(canonical_id.clone()),
        kind: ConflictKind::Unavailable,
        detail: format!(
            "has conflicting exact version requirements: {chosen_version} and {required_version}"
        ),
    }
}

fn installed_entries_incompatible_with<'a>(
    manifest: &'a ContentManifest,
    candidate: &CanonicalId,
) -> Vec<&'a ManifestEntry> {
    manifest
        .entries
        .iter()
        .filter(|entry| entry.canonical_id != *candidate)
        .filter(|entry| {
            entry.dependencies.iter().any(|dependency| {
                dependency.kind == DependencyKind::Incompatible
                    && dependency
                        .project_id
                        .as_deref()
                        .is_some_and(|project_id| project_id == candidate.project_id())
            })
        })
        .collect()
}

fn incompatible_conflict(canonical_id: &CanonicalId, installed: &ManifestEntry) -> PlanConflict {
    let installed_label = installed
        .title
        .clone()
        .unwrap_or_else(|| installed.project_id.clone());
    PlanConflict {
        canonical_id: Some(canonical_id.clone()),
        kind: ConflictKind::Incompatible,
        detail: format!("is incompatible with {installed_label}, which is already installed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn selection(id: &str, kind: ContentKind) -> ContentSelection {
        ContentSelection {
            canonical_id: id.to_string(),
            kind,
            version_id: None,
        }
    }

    fn resolved_item(project_id: &str, incompatible_with: Option<&str>) -> ResolvedItem {
        ResolvedItem {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, project_id),
            provider: ProviderId::Modrinth,
            project_id: project_id.to_string(),
            kind: ContentKind::Mod,
            version_id: "v1".to_string(),
            version_number: "1".to_string(),
            title: project_id.to_string(),
            file: file(&format!("{project_id}.jar"), None),
            dependencies: incompatible_with
                .map(|other| {
                    vec![ContentDependency {
                        project_id: Some(other.to_string()),
                        version_id: None,
                        kind: DependencyKind::Incompatible,
                    }]
                })
                .unwrap_or_default(),
            reason: PlanReason::Selected,
            already_installed: false,
            update: false,
        }
    }

    fn target(supports_mods: bool) -> ResolveTarget {
        ResolveTarget {
            game_dir: None,
            loader: if supports_mods { "fabric" } else { "vanilla" }.to_string(),
            game_version: "1.21.6".to_string(),
            supports_mods,
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
    fn newer_version_flags_only_strictly_newer_picks() {
        let versions = vec![
            version(
                "rel-2",
                ReleaseChannel::Release,
                vec![file("rel2.jar", None)],
            ),
            version(
                "rel-1",
                ReleaseChannel::Release,
                vec![file("rel1.jar", None)],
            ),
        ];
        assert_eq!(newer_version(&versions, "rel-1").unwrap().id, "rel-2");
        assert!(newer_version(&versions, "rel-2").is_none());
    }

    #[test]
    fn newer_version_leaves_a_beta_ahead_of_the_latest_release_alone() {
        let versions = vec![
            version(
                "beta-2",
                ReleaseChannel::Beta,
                vec![file("beta2.jar", None)],
            ),
            version(
                "rel-1",
                ReleaseChannel::Release,
                vec![file("rel1.jar", None)],
            ),
        ];
        assert!(newer_version(&versions, "beta-2").is_none());
    }

    #[test]
    fn newer_version_offers_the_best_pick_when_current_is_not_compatible() {
        let versions = vec![version(
            "rel-1",
            ReleaseChannel::Release,
            vec![file("rel1.jar", None)],
        )];
        assert_eq!(newer_version(&versions, "old-gone").unwrap().id, "rel-1");
        assert!(newer_version(&[], "old-gone").is_none());
    }

    #[test]
    fn a_version_conflicts_only_with_other_installed_content() {
        let mut update = version("v1", ReleaseChannel::Release, vec![file("a.jar", None)]);
        update.dependencies.push(ContentDependency {
            project_id: Some("XXX".to_string()),
            version_id: None,
            kind: DependencyKind::Incompatible,
        });
        let own = CanonicalId::for_project(ProviderId::Modrinth, "SELF");
        let other = CanonicalId::for_project(ProviderId::Modrinth, "XXX");
        let installed = vec![ManifestEntry::managed(
            other.clone(),
            ProviderId::Modrinth,
            "XXX".to_string(),
            "v1".to_string(),
            ContentKind::Mod,
            &file("other.jar", None),
            Vec::new(),
            Some("Other".to_string()),
        )];

        assert!(version_conflicts_with_installed(&update, &own, &installed));
        assert!(
            !version_conflicts_with_installed(&update, &other, &installed),
            "the entry being replaced is exempt"
        );
        assert!(!version_conflicts_with_installed(&update, &own, &[]));
    }

    #[test]
    fn installed_content_can_declare_the_candidate_incompatible() {
        let candidate = CanonicalId::for_project(ProviderId::Modrinth, "candidate");
        let installed_id = CanonicalId::for_project(ProviderId::Modrinth, "installed");
        let installed = vec![ManifestEntry::managed(
            installed_id,
            ProviderId::Modrinth,
            "installed".to_string(),
            "v1".to_string(),
            ContentKind::Mod,
            &file("installed.jar", None),
            vec![ContentDependency {
                project_id: Some("candidate".to_string()),
                version_id: None,
                kind: DependencyKind::Incompatible,
            }],
            Some("Installed".to_string()),
        )];
        let update = version(
            "candidate-v2",
            ReleaseChannel::Release,
            vec![file("candidate.jar", None)],
        );

        assert!(version_conflicts_with_installed(
            &update, &candidate, &installed
        ));

        let manifest = ContentManifest {
            entries: installed,
            ..ContentManifest::default()
        };
        assert_eq!(
            installed_entries_incompatible_with(&manifest, &candidate).len(),
            1
        );
    }

    #[test]
    fn conflicting_exact_dependency_versions_are_unavailable() {
        let dependency = CanonicalId::for_project(ProviderId::Modrinth, "dependency");
        let conflict = exact_dependency_conflict(&dependency, "version-a", "version-b");

        assert_eq!(conflict.kind, ConflictKind::Unavailable);
        assert_eq!(conflict.canonical_id, Some(dependency));
        assert!(conflict.detail.contains("version-a"));
        assert!(conflict.detail.contains("version-b"));
    }

    #[test]
    fn newly_discovered_exact_requirement_replans_only_unpinned_content() {
        let dependency = CanonicalId::for_project(ProviderId::Modrinth, "dependency");
        assert!(exact_requirement_needs_retry(&HashMap::new(), &dependency));

        let pinned = HashMap::from([(dependency.clone(), "version-a".to_string())]);
        assert!(
            !exact_requirement_needs_retry(&pinned, &dependency),
            "a second exact requirement must be reported as a conflict"
        );
    }

    #[test]
    fn selected_content_is_checked_for_mutual_incompatibility() {
        let items = vec![
            resolved_item("first", Some("second")),
            resolved_item("second", None),
        ];

        let conflicts = selected_incompatibility_conflicts(&items);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::Incompatible);
        assert_eq!(
            conflicts[0].canonical_id.as_ref().map(CanonicalId::as_str),
            Some("modrinth:first")
        );
    }

    #[test]
    fn a_vanilla_target_takes_packs_but_refuses_mods() {
        let vanilla = target(false);

        for kind in [ContentKind::ResourcePack, ContentKind::ShaderPack] {
            assert!(
                require_installable_kind(kind, &vanilla).is_ok(),
                "{kind:?} needs no mod loader"
            );
        }
        assert!(require_installable_kind(ContentKind::Mod, &vanilla).is_err());
    }

    #[test]
    fn a_modded_target_takes_every_installable_kind() {
        let fabric = target(true);
        for kind in [
            ContentKind::Mod,
            ContentKind::ResourcePack,
            ContentKind::ShaderPack,
        ] {
            assert!(require_installable_kind(kind, &fabric).is_ok());
        }
    }

    #[test]
    fn modpacks_are_never_added_to_an_instance() {
        assert!(require_installable_kind(ContentKind::Modpack, &target(true)).is_err());
    }

    #[test]
    fn selected_kind_must_match_provider_metadata() {
        let id = CanonicalId::for_project(ProviderId::Modrinth, "project");
        let metadata = HashMap::from([(
            id,
            ProjectMetadata {
                kind: ContentKind::Mod,
                title: "Project".to_string(),
            },
        )]);
        let forged = selection("modrinth:project", ContentKind::ResourcePack);

        assert!(validate_selected_kinds(&[forged], &metadata, &target(true)).is_err());
        assert!(validate_selected_kinds(&[], &metadata, &target(true)).is_err());
    }

    #[test]
    fn stale_manifest_entry_is_not_treated_as_installed() {
        let root = std::env::temp_dir().join(format!(
            "axial-resolve-stale-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("mods")).expect("mods");
        let mut entry = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "project"),
            ProviderId::Modrinth,
            "project".to_string(),
            "v1".to_string(),
            ContentKind::Mod,
            &file("project.jar", None),
            Vec::new(),
            None,
        );

        assert_eq!(
            resolved_install_state(Some(&entry), Some(&root), "v1"),
            (false, false)
        );
        let path = root.join("mods/project.jar");
        std::fs::write(&path, b"jar").expect("managed file");
        entry.sha512 = Some(axial_content::sha512_file(&path).expect("managed hash"));
        entry.size = Some(3);
        assert_eq!(
            resolved_install_state(Some(&entry), Some(&root), "v1"),
            (true, false)
        );
        let _ = std::fs::remove_dir_all(root);
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

        assert_eq!(resolution.to_install().len(), 2);

        let plan = resolution.into_plan(Some("inst".to_string()), &target(true));
        assert_eq!(plan.total_download_bytes, 300);
        assert_eq!(plan.items.len(), 3);
    }
}
