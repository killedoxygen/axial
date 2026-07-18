//! Resolve provider-authored content selections into a concrete dependency
//! closure. The engine owns graph traversal, exact dependency stabilization,
//! installed-content conflicts and version choice; callers adapt the domain
//! result to their transport and execution surfaces.

use crate::{
    CanonicalId, ContentDependency, ContentError, ContentKind, ContentManifest, ContentRegistry,
    ContentVersion, DependencyKind, FileRef, LoaderGameFilter, ManifestEntry, PlannedFile,
    ProjectMetadata, ProviderId, ReleaseChannel, VersionIdentity, entry_file_present,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_RESOLVE_ITEMS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionReason {
    Selected,
    Dependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionConflictKind {
    Unavailable,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionConflictReason {
    NoCompatibleVersion,
    DependencyGraphTooLarge,
    RequiredDependencyUnidentified,
    StabilizationFailed,
    ExactVersionConflict {
        chosen_version_id: String,
        required_version_id: String,
    },
    SelectedIncompatibility {
        other_project_id: String,
    },
    InstalledIncompatibility {
        installed_project_id: String,
        installed_title: Option<String>,
    },
}

impl ResolutionConflictReason {
    pub fn kind(&self) -> ResolutionConflictKind {
        match self {
            Self::SelectedIncompatibility { .. } | Self::InstalledIncompatibility { .. } => {
                ResolutionConflictKind::Incompatible
            }
            Self::NoCompatibleVersion
            | Self::DependencyGraphTooLarge
            | Self::RequiredDependencyUnidentified
            | Self::StabilizationFailed
            | Self::ExactVersionConflict { .. } => ResolutionConflictKind::Unavailable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionConflict {
    pub canonical_id: Option<CanonicalId>,
    /// Raw provider-authored title for an Application adapter to bound and
    /// sanitize. Core never formats it into public copy.
    pub subject_title: Option<String>,
    pub reason: ResolutionConflictReason,
}

impl ResolutionConflict {
    pub fn kind(&self) -> ResolutionConflictKind {
        self.reason.kind()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionTarget {
    /// `None` for a draft target that has no manifest or managed files yet.
    pub game_dir: Option<PathBuf>,
    pub loader: String,
    pub game_version: String,
    pub supports_mods: bool,
}

impl ResolutionTarget {
    /// Provider version filter for the content kind. Resource and shader packs
    /// are not tagged with the instance's mod loader upstream.
    pub fn filter_for(&self, kind: ContentKind) -> LoaderGameFilter {
        LoaderGameFilter {
            loader: (kind.filters_by_loader() && self.supports_mods).then(|| self.loader.clone()),
            game_version: Some(self.game_version.clone()).filter(|value| !value.is_empty()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionSelection {
    pub canonical_id: String,
    pub kind: ContentKind,
    pub version_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error("no content selected")]
    NoSelection,
    #[error("the selected content type changed")]
    SelectedKindChanged,
    #[error("a modpack cannot be added to an instance")]
    ModpackRequiresInstance,
    #[error("the target does not support mods")]
    ModLoaderRequired,
    #[error("an installed exact dependency could not be identified")]
    InstalledDependencyUnidentified,
    #[error(transparent)]
    Provider(#[from] ContentError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContentItem {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub kind: ContentKind,
    pub version_id: String,
    pub version_number: String,
    pub title: String,
    pub file: FileRef,
    pub dependencies: Vec<ContentDependency>,
    pub reason: ResolutionReason,
    pub already_installed: bool,
    pub update: bool,
}

impl ResolvedContentItem {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentResolution {
    pub items: Vec<ResolvedContentItem>,
    pub conflicts: Vec<ResolutionConflict>,
}

impl ContentResolution {
    /// Files that actually need downloading: what is already installed at the
    /// resolved version is left alone.
    pub fn to_install(&self) -> Vec<PlannedFile> {
        self.items
            .iter()
            .filter(|item| !item.already_installed || item.update)
            .map(ResolvedContentItem::to_planned)
            .collect()
    }
}

/// Reject a provider-authored type the target cannot physically accept.
fn require_installable_kind(
    kind: ContentKind,
    target: &ResolutionTarget,
) -> Result<(), ResolutionError> {
    if kind == ContentKind::Modpack {
        return Err(ResolutionError::ModpackRequiresInstance);
    }
    if !target.supports_mods && kind.requires_mod_loader() {
        return Err(ResolutionError::ModLoaderRequired);
    }
    Ok(())
}

fn validate_selected_kinds(
    selections: &[ResolutionSelection],
    metadata: &HashMap<CanonicalId, ProjectMetadata>,
    target: &ResolutionTarget,
) -> Result<(), ResolutionError> {
    if selections.is_empty() {
        return Err(ResolutionError::NoSelection);
    }
    for selection in selections {
        let canonical_id = CanonicalId(selection.canonical_id.clone());
        let Some(authoritative) = metadata.get(&canonical_id) else {
            continue;
        };
        if selection.kind != authoritative.kind {
            return Err(ResolutionError::SelectedKindChanged);
        }
        require_installable_kind(authoritative.kind, target)?;
    }
    Ok(())
}

struct ResolvePass {
    resolution: ContentResolution,
    retry_with_exact: Option<(CanonicalId, String)>,
}

pub async fn resolve_content(
    registry: &ContentRegistry,
    target: &ResolutionTarget,
    selections: &[ResolutionSelection],
    manifest: &ContentManifest,
) -> Result<ContentResolution, ResolutionError> {
    if selections.is_empty() {
        return Err(ResolutionError::NoSelection);
    }
    let mut exact_requirements = HashMap::new();
    let replacing: HashSet<CanonicalId> = selections
        .iter()
        .map(|selection| CanonicalId(selection.canonical_id.clone()))
        .collect();
    for (canonical_id, version_id) in
        installed_exact_requirements(registry, target, manifest, &replacing).await?
    {
        if let Some(conflict) =
            insert_exact_requirement(&mut exact_requirements, canonical_id, version_id)
        {
            return Ok(ContentResolution {
                items: Vec::new(),
                conflicts: vec![conflict],
            });
        }
    }
    for selection in selections {
        let Some(version_id) = selection.version_id.as_ref() else {
            continue;
        };
        let canonical_id = CanonicalId(selection.canonical_id.clone());
        if let Some(conflict) =
            insert_exact_requirement(&mut exact_requirements, canonical_id, version_id.clone())
        {
            return Ok(ContentResolution {
                items: Vec::new(),
                conflicts: vec![conflict],
            });
        }
    }

    for _ in 0..MAX_RESOLVE_ITEMS {
        let pass =
            resolve_pass(registry, target, selections, manifest, &exact_requirements).await?;
        let Some((canonical_id, required_version)) = pass.retry_with_exact else {
            return Ok(pass.resolution);
        };
        exact_requirements.insert(canonical_id, required_version);
    }

    Ok(ContentResolution {
        items: Vec::new(),
        conflicts: vec![ResolutionConflict {
            canonical_id: None,
            subject_title: None,
            reason: ResolutionConflictReason::StabilizationFailed,
        }],
    })
}

fn insert_exact_requirement(
    requirements: &mut HashMap<CanonicalId, String>,
    canonical_id: CanonicalId,
    version_id: String,
) -> Option<ResolutionConflict> {
    let previous = requirements.insert(canonical_id.clone(), version_id.clone())?;
    (previous != version_id)
        .then(|| exact_dependency_conflict(&canonical_id, &previous, &version_id))
}

async fn installed_exact_requirements(
    registry: &ContentRegistry,
    target: &ResolutionTarget,
    manifest: &ContentManifest,
    replacing: &HashSet<CanonicalId>,
) -> Result<Vec<(CanonicalId, String)>, ResolutionError> {
    let live_entries: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|entry| !replacing.contains(&entry.canonical_id))
        .filter(|entry| installed_entry_present(entry, target.game_dir.as_deref()))
        .collect();
    let installed_versions: HashMap<&str, &ManifestEntry> = live_entries
        .iter()
        .map(|entry| (entry.version_id.as_str(), *entry))
        .collect();
    let unresolved_version_ids: Vec<String> = live_entries
        .iter()
        .flat_map(|entry| entry.dependencies.iter())
        .filter(|dependency| dependency.kind == DependencyKind::Required)
        .filter(|dependency| dependency.project_id.is_none())
        .filter_map(|dependency| dependency.version_id.as_deref())
        .filter(|version_id| !installed_versions.contains_key(version_id))
        .map(str::to_string)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let version_identities = if unresolved_version_ids.is_empty() {
        HashMap::new()
    } else {
        registry.version_identities(&unresolved_version_ids).await?
    };

    let mut requirements = Vec::new();
    for entry in live_entries {
        for dependency in entry
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Required)
        {
            let Some(version_id) = dependency.version_id.clone() else {
                continue;
            };
            let canonical_id = match dependency.project_id.as_deref() {
                Some(project_id) => CanonicalId::for_project(ProviderId::Modrinth, project_id),
                None => installed_versions
                    .get(version_id.as_str())
                    .map(|installed| installed.canonical_id.clone())
                    .or_else(|| {
                        version_identities.get(&version_id).map(|identity| {
                            CanonicalId::for_project(identity.provider, &identity.project_id)
                        })
                    })
                    .ok_or(ResolutionError::InstalledDependencyUnidentified)?,
            };
            requirements.push((canonical_id, version_id));
        }
    }
    Ok(requirements)
}

async fn resolve_pass(
    registry: &ContentRegistry,
    target: &ResolutionTarget,
    selections: &[ResolutionSelection],
    manifest: &ContentManifest,
    exact_requirements: &HashMap<CanonicalId, String>,
) -> Result<ResolvePass, ResolutionError> {
    let selected_ids: Vec<CanonicalId> = selections
        .iter()
        .map(|selection| CanonicalId(selection.canonical_id.clone()))
        .collect();
    let mut metadata = registry.metadata(&selected_ids).await?;
    validate_selected_kinds(selections, &metadata, target)?;
    let mut metadata_requested: HashSet<CanonicalId> = selected_ids.into_iter().collect();
    let mut resolved_versions: HashMap<CanonicalId, String> = HashMap::new();
    let mut items: Vec<ResolvedContentItem> = Vec::new();
    let mut conflicts: Vec<ResolutionConflict> = Vec::new();
    let mut incompatibilities: HashSet<(CanonicalId, CanonicalId)> = HashSet::new();
    let mut dependency_versions: HashMap<String, VersionIdentity> = HashMap::new();
    let mut dependency_versions_requested: HashSet<String> = HashSet::new();

    let mut queue: VecDeque<(CanonicalId, Option<String>, ResolutionReason)> = selections
        .iter()
        .map(|selection| {
            let canonical_id = CanonicalId(selection.canonical_id.clone());
            let version_id = selection
                .version_id
                .clone()
                .or_else(|| exact_requirements.get(&canonical_id).cloned());
            (canonical_id, version_id, ResolutionReason::Selected)
        })
        .collect();

    while let Some((canonical_id, forced_version, reason)) = queue.pop_front() {
        let pinned_version = exact_requirements.get(&canonical_id);
        if let (Some(forced_version), Some(pinned_version)) =
            (forced_version.as_deref(), pinned_version)
            && forced_version != pinned_version
        {
            conflicts.push(exact_dependency_conflict(
                &canonical_id,
                forced_version,
                pinned_version,
            ));
            continue;
        }
        let forced_version = forced_version.or_else(|| pinned_version.cloned());
        if let Some(chosen_version) = resolved_versions.get(&canonical_id) {
            if let Some(required_version) = forced_version.as_deref()
                && required_version != chosen_version
            {
                if exact_requirement_needs_retry(exact_requirements, &canonical_id) {
                    return Ok(ResolvePass {
                        resolution: ContentResolution { items, conflicts },
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
            conflicts.push(ResolutionConflict {
                canonical_id: Some(canonical_id),
                subject_title: None,
                reason: ResolutionConflictReason::DependencyGraphTooLarge,
            });
            break;
        }

        if !metadata_requested.contains(&canonical_id) {
            let mut missing_ids = HashSet::new();
            missing_ids.insert(canonical_id.clone());
            missing_ids.extend(
                queue
                    .iter()
                    .filter(|(queued_id, _, _)| !metadata_requested.contains(queued_id))
                    .map(|(queued_id, _, _)| queued_id.clone()),
            );
            let missing_ids: Vec<CanonicalId> = missing_ids.into_iter().collect();
            let fetched = registry.metadata(&missing_ids).await?;
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
        let versions = match registry.versions(&canonical_id, &filter).await {
            Ok(versions) => versions,
            Err(ContentError::Status { status, .. }) if status.as_u16() == 404 => {
                conflicts.push(unavailable_conflict(&canonical_id));
                continue;
            }
            Err(error) => return Err(ResolutionError::Provider(error)),
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

        let missing_dependency_versions: Vec<String> = version
            .dependencies
            .iter()
            .filter(|dependency| dependency.project_id.is_none())
            .filter_map(|dependency| dependency.version_id.clone())
            .filter(|version_id| dependency_versions_requested.insert(version_id.clone()))
            .collect();
        if !missing_dependency_versions.is_empty() {
            dependency_versions.extend(
                registry
                    .version_identities(&missing_dependency_versions)
                    .await?,
            );
        }
        let dependencies =
            canonicalize_version_only_dependencies(&version.dependencies, &dependency_versions);

        for dependency in &dependencies {
            match dependency.kind {
                DependencyKind::Required => {
                    if let Some(project_id) = &dependency.project_id {
                        queue.push_back((
                            CanonicalId::for_project(ProviderId::Modrinth, project_id),
                            dependency.version_id.clone(),
                            ResolutionReason::Dependency,
                        ));
                    } else {
                        conflicts.push(ResolutionConflict {
                            canonical_id: Some(canonical_id.clone()),
                            subject_title: None,
                            reason: ResolutionConflictReason::RequiredDependencyUnidentified,
                        });
                    }
                }
                DependencyKind::Incompatible => {
                    if let Some(project_id) = &dependency.project_id {
                        let incompatible =
                            CanonicalId::for_project(ProviderId::Modrinth, project_id);
                        if let Some(entry) = manifest.find(&incompatible)
                            && installed_entry_present(entry, target.game_dir.as_deref())
                            && incompatible_dependency_matches(
                                dependency,
                                &entry.project_id,
                                &entry.version_id,
                            )
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

        for entry in installed_entries_incompatible_with(
            manifest,
            &canonical_id,
            &version.id,
            target.game_dir.as_deref(),
        ) {
            if incompatibilities.insert((canonical_id.clone(), entry.canonical_id.clone())) {
                conflicts.push(incompatible_conflict(&canonical_id, entry));
            }
        }

        let existing = manifest.find(&canonical_id);
        let (already_installed, update) =
            resolved_install_state(existing, target.game_dir.as_deref(), &version.id);
        let project_id = canonical_id.project_id().to_string();

        items.push(ResolvedContentItem {
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
            dependencies,
            reason,
            already_installed,
            update,
        });
    }

    conflicts.extend(selected_incompatibility_conflicts(&items));

    apply_metadata_titles(&metadata, &mut conflicts);
    Ok(ResolvePass {
        resolution: ContentResolution { items, conflicts },
        retry_with_exact: None,
    })
}

pub fn canonicalize_version_only_dependencies(
    dependencies: &[ContentDependency],
    versions: &HashMap<String, VersionIdentity>,
) -> Vec<ContentDependency> {
    dependencies
        .iter()
        .cloned()
        .map(|mut dependency| {
            if dependency.project_id.is_none()
                && let Some(identity) = dependency
                    .version_id
                    .as_ref()
                    .and_then(|version_id| versions.get(version_id))
            {
                dependency.project_id = Some(identity.project_id.clone());
            }
            dependency
        })
        .collect()
}

pub fn has_unresolved_version_only_incompatibility(dependencies: &[ContentDependency]) -> bool {
    dependencies.iter().any(|dependency| {
        dependency.kind == DependencyKind::Incompatible
            && dependency.project_id.is_none()
            && dependency.version_id.is_some()
    })
}

fn incompatible_dependency_matches(
    dependency: &ContentDependency,
    project_id: &str,
    version_id: &str,
) -> bool {
    if dependency.kind != DependencyKind::Incompatible {
        return false;
    }
    match dependency.project_id.as_deref() {
        Some(exact_project) => {
            exact_project == project_id
                && dependency
                    .version_id
                    .as_deref()
                    .is_none_or(|exact_version| exact_version == version_id)
        }
        None => dependency.version_id.as_deref() == Some(version_id),
    }
}

fn exact_requirement_needs_retry(
    exact_requirements: &HashMap<CanonicalId, String>,
    canonical_id: &CanonicalId,
) -> bool {
    !exact_requirements.contains_key(canonical_id)
}

fn resolved_install_state(
    existing: Option<&ManifestEntry>,
    game_dir: Option<&Path>,
    resolved_version_id: &str,
) -> (bool, bool) {
    let already_installed = existing.is_some_and(|entry| installed_entry_present(entry, game_dir));
    let update =
        already_installed && existing.is_some_and(|entry| entry.version_id != resolved_version_id);
    (already_installed, update)
}

fn installed_entry_present(entry: &ManifestEntry, game_dir: Option<&Path>) -> bool {
    game_dir.is_none_or(|root| entry_file_present(root, entry))
}

fn selected_incompatibility_conflicts(items: &[ResolvedContentItem]) -> Vec<ResolutionConflict> {
    let resolved_projects: HashSet<String> =
        items.iter().map(|item| item.project_id.clone()).collect();
    let mut selected_incompatibilities = HashSet::new();
    let mut conflicts = Vec::new();
    for item in items {
        for dependency in &item.dependencies {
            let Some(project_id) = dependency.project_id.as_deref() else {
                continue;
            };
            if !resolved_projects.contains(project_id) {
                continue;
            }
            if !items.iter().any(|candidate| {
                incompatible_dependency_matches(
                    dependency,
                    &candidate.project_id,
                    &candidate.version_id,
                )
            }) {
                continue;
            }
            let key = (item.project_id.clone(), project_id.to_string());
            if selected_incompatibilities.insert(key) {
                conflicts.push(ResolutionConflict {
                    canonical_id: Some(item.canonical_id.clone()),
                    subject_title: None,
                    reason: ResolutionConflictReason::SelectedIncompatibility {
                        other_project_id: project_id.to_string(),
                    },
                });
            }
        }
    }
    conflicts
}

fn apply_metadata_titles(
    metadata: &HashMap<CanonicalId, ProjectMetadata>,
    conflicts: &mut [ResolutionConflict],
) {
    for conflict in conflicts {
        if let Some(id) = &conflict.canonical_id {
            conflict.subject_title = metadata.get(id).map(|project| project.title.clone());
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
/// or violate an installed dependent's exact requirement. The entry being
/// updated is exempt from incompatibility checks because it is being replaced.
pub fn version_conflicts_with_installed(
    version: &ContentVersion,
    own_id: &CanonicalId,
    installed: &[ManifestEntry],
) -> bool {
    let current_version_id = installed
        .iter()
        .find(|entry| entry.canonical_id == *own_id)
        .map(|entry| entry.version_id.as_str())
        .unwrap_or("");
    let candidate_declares_conflict = version.dependencies.iter().any(|dependency| {
        installed.iter().any(|entry| {
            entry.canonical_id != *own_id
                && incompatible_dependency_matches(dependency, &entry.project_id, &entry.version_id)
        })
    });
    if candidate_declares_conflict {
        return true;
    }

    installed.iter().any(|entry| {
        entry.canonical_id != *own_id
            && entry.dependencies.iter().any(|dependency| {
                incompatible_dependency_matches(dependency, own_id.project_id(), &version.id)
                    || dependency.rejects_required_version(
                        own_id.project_id(),
                        current_version_id,
                        &version.id,
                    )
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

fn unavailable_conflict(canonical_id: &CanonicalId) -> ResolutionConflict {
    ResolutionConflict {
        canonical_id: Some(canonical_id.clone()),
        subject_title: None,
        reason: ResolutionConflictReason::NoCompatibleVersion,
    }
}

fn exact_dependency_conflict(
    canonical_id: &CanonicalId,
    chosen_version: &str,
    required_version: &str,
) -> ResolutionConflict {
    ResolutionConflict {
        canonical_id: Some(canonical_id.clone()),
        subject_title: None,
        reason: ResolutionConflictReason::ExactVersionConflict {
            chosen_version_id: chosen_version.to_string(),
            required_version_id: required_version.to_string(),
        },
    }
}

fn installed_entries_incompatible_with<'a>(
    manifest: &'a ContentManifest,
    candidate: &CanonicalId,
    candidate_version_id: &str,
    game_dir: Option<&Path>,
) -> Vec<&'a ManifestEntry> {
    manifest
        .entries
        .iter()
        .filter(|entry| entry.canonical_id != *candidate)
        .filter(|entry| installed_entry_present(entry, game_dir))
        .filter(|entry| {
            entry.dependencies.iter().any(|dependency| {
                incompatible_dependency_matches(
                    dependency,
                    candidate.project_id(),
                    candidate_version_id,
                )
            })
        })
        .collect()
}

fn incompatible_conflict(
    canonical_id: &CanonicalId,
    installed: &ManifestEntry,
) -> ResolutionConflict {
    ResolutionConflict {
        canonical_id: Some(canonical_id.clone()),
        subject_title: None,
        reason: ResolutionConflictReason::InstalledIncompatibility {
            installed_project_id: installed.project_id.clone(),
            installed_title: installed.title.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    enum ProviderResponse {
        Json(Value),
        Status(u16),
    }

    struct ProviderFixture {
        registry: ContentRegistry,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ProviderFixture {
        async fn new(projects: Vec<Value>, versions: HashMap<String, ProviderResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind content provider fixture");
            let address = listener
                .local_addr()
                .expect("content provider fixture address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded_requests = Arc::clone(&requests);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let projects = projects.clone();
                    let versions = versions.clone();
                    let recorded_requests = Arc::clone(&recorded_requests);
                    tokio::spawn(async move {
                        let mut request = Vec::with_capacity(2048);
                        let mut chunk = [0_u8; 1024];
                        while request.len() < 16 * 1024 {
                            let Ok(read) = socket.read(&mut chunk).await else {
                                return;
                            };
                            if read == 0 {
                                break;
                            }
                            request.extend_from_slice(&chunk[..read]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let request = String::from_utf8_lossy(&request);
                        let target = request
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .to_string();
                        recorded_requests
                            .lock()
                            .expect("record provider request")
                            .push(target.clone());

                        let path = target.split('?').next().unwrap_or(&target);
                        let response = if path == "/v2/projects" {
                            ProviderResponse::Json(Value::Array(projects))
                        } else if let Some(project_id) = path
                            .strip_prefix("/v2/project/")
                            .and_then(|path| path.strip_suffix("/version"))
                        {
                            versions
                                .get(project_id)
                                .cloned()
                                .unwrap_or(ProviderResponse::Status(404))
                        } else {
                            ProviderResponse::Status(404)
                        };
                        let (status, reason, body) = match response {
                            ProviderResponse::Json(value) => (
                                200,
                                "OK",
                                serde_json::to_vec(&value).expect("encode provider fixture"),
                            ),
                            ProviderResponse::Status(404) => {
                                (404, "Not Found", b"not found".to_vec())
                            }
                            ProviderResponse::Status(503) => {
                                (503, "Service Unavailable", b"unavailable".to_vec())
                            }
                            ProviderResponse::Status(status) => {
                                (status, "Error", b"provider error".to_vec())
                            }
                        };
                        let headers = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        if socket.write_all(headers.as_bytes()).await.is_ok() {
                            let _ = socket.write_all(&body).await;
                        }
                    });
                }
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("content provider fixture client");
            let provider = crate::modrinth::ModrinthProvider::with_base_url(
                client.clone(),
                format!("http://{address}/v2"),
            );
            let registry = ContentRegistry::with_modrinth(client, provider);
            Self {
                registry,
                requests,
                task,
            }
        }

        fn request_count(&self, path: &str) -> usize {
            self.requests
                .lock()
                .expect("read provider requests")
                .iter()
                .filter(|request| request.split('?').next() == Some(path))
                .count()
        }
    }

    impl Drop for ProviderFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn provider_project(id: &str, title: &str) -> Value {
        json!({
            "id": id,
            "title": title,
            "project_type": "mod"
        })
    }

    fn provider_dependency(
        project_id: &str,
        version_id: Option<&str>,
        dependency_type: &str,
    ) -> Value {
        json!({
            "project_id": project_id,
            "version_id": version_id,
            "dependency_type": dependency_type
        })
    }

    fn provider_version(id: &str, project_id: &str, dependencies: Vec<Value>) -> Value {
        json!({
            "id": id,
            "project_id": project_id,
            "name": format!("{project_id} {id}"),
            "version_number": id,
            "version_type": "release",
            "game_versions": ["1.21.11"],
            "loaders": ["fabric"],
            "dependencies": dependencies,
            "files": [{
                "hashes": { "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
                "url": format!("https://example.invalid/{project_id}.jar"),
                "filename": format!("{project_id}.jar"),
                "primary": true,
                "size": 1
            }]
        })
    }

    fn provider_versions(versions: Vec<Value>) -> ProviderResponse {
        ProviderResponse::Json(Value::Array(versions))
    }

    fn resolver_target() -> ResolutionTarget {
        ResolutionTarget {
            game_dir: None,
            loader: "fabric".to_string(),
            game_version: "1.21.11".to_string(),
            supports_mods: true,
        }
    }

    #[tokio::test]
    async fn resolve_content_expands_the_required_dependency_closure() {
        let fixture = ProviderFixture::new(
            vec![
                provider_project("root", "Root"),
                provider_project("dependency", "Dependency"),
            ],
            HashMap::from([
                (
                    "root".to_string(),
                    provider_versions(vec![provider_version(
                        "root-v1",
                        "root",
                        vec![provider_dependency("dependency", None, "required")],
                    )]),
                ),
                (
                    "dependency".to_string(),
                    provider_versions(vec![provider_version(
                        "dependency-v1",
                        "dependency",
                        Vec::new(),
                    )]),
                ),
            ]),
        )
        .await;

        let resolution = resolve_content(
            &fixture.registry,
            &resolver_target(),
            &[selection("modrinth:root", ContentKind::Mod)],
            &ContentManifest::default(),
        )
        .await
        .expect("resolve dependency closure");

        assert!(resolution.conflicts.is_empty());
        assert_eq!(resolution.items.len(), 2);
        assert_eq!(resolution.items[0].project_id, "root");
        assert_eq!(resolution.items[0].reason, ResolutionReason::Selected);
        assert_eq!(resolution.items[1].project_id, "dependency");
        assert_eq!(resolution.items[1].reason, ResolutionReason::Dependency);
    }

    #[tokio::test]
    async fn resolve_content_retries_until_a_late_exact_requirement_stabilizes() {
        let fixture = ProviderFixture::new(
            vec![
                provider_project("root", "Root"),
                provider_project("pinning", "Pinning"),
                provider_project("dependency", "Dependency"),
            ],
            HashMap::from([
                (
                    "root".to_string(),
                    provider_versions(vec![provider_version(
                        "root-v1",
                        "root",
                        vec![provider_dependency("dependency", None, "required")],
                    )]),
                ),
                (
                    "pinning".to_string(),
                    provider_versions(vec![provider_version(
                        "pinning-v1",
                        "pinning",
                        vec![provider_dependency(
                            "dependency",
                            Some("dependency-v1"),
                            "required",
                        )],
                    )]),
                ),
                (
                    "dependency".to_string(),
                    provider_versions(vec![
                        provider_version("dependency-v2", "dependency", Vec::new()),
                        provider_version("dependency-v1", "dependency", Vec::new()),
                    ]),
                ),
            ]),
        )
        .await;

        let resolution = resolve_content(
            &fixture.registry,
            &resolver_target(),
            &[
                selection("modrinth:root", ContentKind::Mod),
                selection("modrinth:pinning", ContentKind::Mod),
            ],
            &ContentManifest::default(),
        )
        .await
        .expect("stabilize exact dependency");

        assert!(resolution.conflicts.is_empty());
        let dependency = resolution
            .items
            .iter()
            .find(|item| item.project_id == "dependency")
            .expect("resolved dependency");
        assert_eq!(dependency.version_id, "dependency-v1");
        assert_eq!(
            fixture.request_count("/v2/project/dependency/version"),
            2,
            "the late exact requirement must trigger one complete retry"
        );
    }

    #[tokio::test]
    async fn resolve_content_reports_conflicting_exact_pins_after_retry() {
        let fixture = ProviderFixture::new(
            vec![
                provider_project("root-a", "Root A"),
                provider_project("root-b", "Root B"),
                provider_project("dependency", "Dependency"),
            ],
            HashMap::from([
                (
                    "root-a".to_string(),
                    provider_versions(vec![provider_version(
                        "root-a-v1",
                        "root-a",
                        vec![provider_dependency(
                            "dependency",
                            Some("dependency-v1"),
                            "required",
                        )],
                    )]),
                ),
                (
                    "root-b".to_string(),
                    provider_versions(vec![provider_version(
                        "root-b-v1",
                        "root-b",
                        vec![provider_dependency(
                            "dependency",
                            Some("dependency-v2"),
                            "required",
                        )],
                    )]),
                ),
                (
                    "dependency".to_string(),
                    provider_versions(vec![
                        provider_version("dependency-v2", "dependency", Vec::new()),
                        provider_version("dependency-v1", "dependency", Vec::new()),
                    ]),
                ),
            ]),
        )
        .await;

        let resolution = resolve_content(
            &fixture.registry,
            &resolver_target(),
            &[
                selection("modrinth:root-a", ContentKind::Mod),
                selection("modrinth:root-b", ContentKind::Mod),
            ],
            &ContentManifest::default(),
        )
        .await
        .expect("resolve conflicting pins");

        assert_eq!(resolution.conflicts.len(), 1);
        assert_eq!(
            resolution.conflicts[0].reason,
            ResolutionConflictReason::ExactVersionConflict {
                chosen_version_id: "dependency-v1".to_string(),
                required_version_id: "dependency-v2".to_string(),
            }
        );
        assert_eq!(
            resolution.conflicts[0].subject_title.as_deref(),
            Some("Dependency")
        );
        assert_eq!(
            resolution.conflicts[0].kind(),
            ResolutionConflictKind::Unavailable
        );
    }

    #[tokio::test]
    async fn resolve_content_preserves_provider_status_errors() {
        let fixture = ProviderFixture::new(
            vec![provider_project("root", "Root")],
            HashMap::from([("root".to_string(), ProviderResponse::Status(503))]),
        )
        .await;

        let error = resolve_content(
            &fixture.registry,
            &resolver_target(),
            &[selection("modrinth:root", ContentKind::Mod)],
            &ContentManifest::default(),
        )
        .await
        .expect_err("provider status must remain an error");

        assert!(matches!(
            error,
            ResolutionError::Provider(ContentError::Status { status, .. })
                if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
    }

    #[tokio::test]
    async fn resolve_content_preserves_selected_incompatibility_order() {
        let fixture = ProviderFixture::new(
            vec![
                provider_project("first", "First"),
                provider_project("second", "Second"),
            ],
            HashMap::from([
                (
                    "first".to_string(),
                    provider_versions(vec![provider_version(
                        "first-v1",
                        "first",
                        vec![provider_dependency("second", None, "incompatible")],
                    )]),
                ),
                (
                    "second".to_string(),
                    provider_versions(vec![provider_version(
                        "second-v1",
                        "second",
                        vec![provider_dependency("first", None, "incompatible")],
                    )]),
                ),
            ]),
        )
        .await;

        let resolution = resolve_content(
            &fixture.registry,
            &resolver_target(),
            &[
                selection("modrinth:first", ContentKind::Mod),
                selection("modrinth:second", ContentKind::Mod),
            ],
            &ContentManifest::default(),
        )
        .await
        .expect("resolve selected incompatibilities");

        assert_eq!(resolution.conflicts.len(), 2);
        assert_eq!(
            resolution.conflicts[0].reason,
            ResolutionConflictReason::SelectedIncompatibility {
                other_project_id: "second".to_string(),
            }
        );
        assert_eq!(
            resolution.conflicts[0].subject_title.as_deref(),
            Some("First")
        );
        assert_eq!(
            resolution.conflicts[1].reason,
            ResolutionConflictReason::SelectedIncompatibility {
                other_project_id: "first".to_string(),
            }
        );
        assert_eq!(
            resolution.conflicts[1].subject_title.as_deref(),
            Some("Second")
        );
    }

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

    fn selection(id: &str, kind: ContentKind) -> ResolutionSelection {
        ResolutionSelection {
            canonical_id: id.to_string(),
            kind,
            version_id: None,
        }
    }

    fn resolved_item(project_id: &str, incompatible_with: Option<&str>) -> ResolvedContentItem {
        ResolvedContentItem {
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
            reason: ResolutionReason::Selected,
            already_installed: false,
            update: false,
        }
    }

    fn target(supports_mods: bool) -> ResolutionTarget {
        ResolutionTarget {
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
            installed_entries_incompatible_with(&manifest, &candidate, &update.id, None).len(),
            1
        );
    }

    #[test]
    fn reverse_incompatibilities_honor_the_candidate_version() {
        let candidate = CanonicalId::for_project(ProviderId::Modrinth, "candidate");
        let installed = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "installed"),
            ProviderId::Modrinth,
            "installed".to_string(),
            "installed-v1".to_string(),
            ContentKind::Mod,
            &file("installed.jar", None),
            vec![ContentDependency {
                project_id: Some("candidate".to_string()),
                version_id: Some("candidate-v1".to_string()),
                kind: DependencyKind::Incompatible,
            }],
            Some("Installed".to_string()),
        );
        let manifest = ContentManifest {
            entries: vec![installed.clone()],
            ..ContentManifest::default()
        };
        let candidate_v1 = version(
            "candidate-v1",
            ReleaseChannel::Release,
            vec![file("candidate.jar", None)],
        );
        let candidate_v2 = version(
            "candidate-v2",
            ReleaseChannel::Release,
            vec![file("candidate.jar", None)],
        );

        assert!(version_conflicts_with_installed(
            &candidate_v1,
            &candidate,
            std::slice::from_ref(&installed),
        ));
        assert!(!version_conflicts_with_installed(
            &candidate_v2,
            &candidate,
            std::slice::from_ref(&installed),
        ));
        assert_eq!(
            installed_entries_incompatible_with(&manifest, &candidate, "candidate-v1", None,).len(),
            1
        );
        assert!(
            installed_entries_incompatible_with(&manifest, &candidate, "candidate-v2", None,)
                .is_empty()
        );
    }

    #[test]
    fn reverse_exact_requirements_block_dependency_version_replacement() {
        let candidate = CanonicalId::for_project(ProviderId::Modrinth, "candidate");
        let installed_candidate = ManifestEntry::managed(
            candidate.clone(),
            ProviderId::Modrinth,
            "candidate".to_string(),
            "candidate-v1".to_string(),
            ContentKind::Mod,
            &file("candidate.jar", None),
            Vec::new(),
            Some("Candidate".to_string()),
        );
        for version_only in [false, true] {
            let dependent = ManifestEntry::managed(
                CanonicalId::for_project(ProviderId::Modrinth, "dependent"),
                ProviderId::Modrinth,
                "dependent".to_string(),
                "dependent-v1".to_string(),
                ContentKind::Mod,
                &file("dependent.jar", None),
                vec![ContentDependency {
                    project_id: (!version_only).then(|| "candidate".to_string()),
                    version_id: Some("candidate-v1".to_string()),
                    kind: DependencyKind::Required,
                }],
                Some("Dependent".to_string()),
            );
            let installed = [installed_candidate.clone(), dependent];
            let candidate_v1 = version(
                "candidate-v1",
                ReleaseChannel::Release,
                vec![file("candidate.jar", None)],
            );
            let candidate_v2 = version(
                "candidate-v2",
                ReleaseChannel::Release,
                vec![file("candidate.jar", None)],
            );

            assert!(!version_conflicts_with_installed(
                &candidate_v1,
                &candidate,
                &installed,
            ));
            assert!(version_conflicts_with_installed(
                &candidate_v2,
                &candidate,
                &installed,
            ));
        }
    }

    #[test]
    fn version_only_reverse_incompatibilities_match_the_exact_candidate() {
        let candidate = CanonicalId::for_project(ProviderId::Modrinth, "candidate");
        let installed = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "installed"),
            ProviderId::Modrinth,
            "installed".to_string(),
            "installed-v1".to_string(),
            ContentKind::Mod,
            &file("installed.jar", None),
            vec![ContentDependency {
                project_id: None,
                version_id: Some("candidate-v1".to_string()),
                kind: DependencyKind::Incompatible,
            }],
            Some("Installed".to_string()),
        );
        let manifest = ContentManifest {
            entries: vec![installed.clone()],
            ..ContentManifest::default()
        };
        let candidate_v1 = version(
            "candidate-v1",
            ReleaseChannel::Release,
            vec![file("candidate.jar", None)],
        );
        let candidate_v2 = version(
            "candidate-v2",
            ReleaseChannel::Release,
            vec![file("candidate.jar", None)],
        );

        assert!(version_conflicts_with_installed(
            &candidate_v1,
            &candidate,
            std::slice::from_ref(&installed),
        ));
        assert!(!version_conflicts_with_installed(
            &candidate_v2,
            &candidate,
            std::slice::from_ref(&installed),
        ));
        assert_eq!(
            installed_entries_incompatible_with(&manifest, &candidate, "candidate-v1", None,).len(),
            1
        );
        assert!(
            installed_entries_incompatible_with(&manifest, &candidate, "candidate-v2", None,)
                .is_empty()
        );
    }

    #[test]
    fn missing_or_replaced_manifest_entries_do_not_create_conflicts() {
        let root = std::env::temp_dir().join(format!(
            "axial-resolve-stale-conflict-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("mods")).expect("mods");
        let candidate = CanonicalId::for_project(ProviderId::Modrinth, "candidate");
        let path = root.join("mods/installed.jar");
        std::fs::write(&path, b"owned bytes").expect("owned file");
        let mut entry = ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "installed"),
            ProviderId::Modrinth,
            "installed".to_string(),
            "v1".to_string(),
            ContentKind::Mod,
            &file("installed.jar", Some(b"owned bytes".len() as u64)),
            vec![ContentDependency {
                project_id: Some("candidate".to_string()),
                version_id: None,
                kind: DependencyKind::Incompatible,
            }],
            None,
        );
        entry.sha512 = Some(crate::sha512_file(&path).expect("owned hash"));
        let manifest = ContentManifest {
            entries: vec![entry.clone()],
            ..ContentManifest::default()
        };

        assert_eq!(
            installed_entries_incompatible_with(
                &manifest,
                &candidate,
                "candidate-v1",
                Some(&root),
            )
            .len(),
            1
        );
        std::fs::write(&path, b"user replacement").expect("replacement");
        assert!(
            installed_entries_incompatible_with(
                &manifest,
                &candidate,
                "candidate-v1",
                Some(&root),
            )
            .is_empty()
        );
        assert!(!installed_entry_present(&entry, Some(&root)));
        std::fs::remove_file(&path).expect("remove replacement");
        assert!(
            installed_entries_incompatible_with(
                &manifest,
                &candidate,
                "candidate-v1",
                Some(&root),
            )
            .is_empty()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conflicting_exact_dependency_versions_are_unavailable() {
        let dependency = CanonicalId::for_project(ProviderId::Modrinth, "dependency");
        let conflict = exact_dependency_conflict(&dependency, "version-a", "version-b");

        assert_eq!(conflict.kind(), ResolutionConflictKind::Unavailable);
        assert_eq!(conflict.canonical_id, Some(dependency));
        assert_eq!(
            conflict.reason,
            ResolutionConflictReason::ExactVersionConflict {
                chosen_version_id: "version-a".to_string(),
                required_version_id: "version-b".to_string(),
            }
        );
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
    fn version_only_dependencies_are_canonicalized_to_their_projects() {
        let dependencies = vec![
            ContentDependency {
                project_id: None,
                version_id: Some("required-version".to_string()),
                kind: DependencyKind::Required,
            },
            ContentDependency {
                project_id: None,
                version_id: Some("incompatible-version".to_string()),
                kind: DependencyKind::Incompatible,
            },
        ];
        let identity = |project_id: &str, version_id: &str| VersionIdentity {
            provider: ProviderId::Modrinth,
            project_id: project_id.to_string(),
            version_id: version_id.to_string(),
            game_versions: Vec::new(),
            loaders: Vec::new(),
            dependencies: Vec::new(),
            title: None,
        };
        let versions = HashMap::from([
            (
                "required-version".to_string(),
                identity("required-project", "required-version"),
            ),
            (
                "incompatible-version".to_string(),
                identity("incompatible-project", "incompatible-version"),
            ),
        ]);

        let canonical = canonicalize_version_only_dependencies(&dependencies, &versions);

        assert_eq!(canonical[0].project_id.as_deref(), Some("required-project"));
        assert_eq!(
            canonical[1].project_id.as_deref(),
            Some("incompatible-project")
        );
        assert_eq!(canonical[0].version_id.as_deref(), Some("required-version"));
    }

    #[test]
    fn version_only_update_incompatibilities_are_canonicalized_and_exact() {
        let dependency = ContentDependency {
            project_id: None,
            version_id: Some("installed-v1".to_string()),
            kind: DependencyKind::Incompatible,
        };
        assert!(has_unresolved_version_only_incompatibility(
            std::slice::from_ref(&dependency)
        ));
        let identity = VersionIdentity {
            provider: ProviderId::Modrinth,
            project_id: "installed".to_string(),
            version_id: "installed-v1".to_string(),
            game_versions: Vec::new(),
            loaders: Vec::new(),
            dependencies: Vec::new(),
            title: None,
        };
        let dependencies = canonicalize_version_only_dependencies(
            &[dependency],
            &HashMap::from([("installed-v1".to_string(), identity)]),
        );
        assert!(!has_unresolved_version_only_incompatibility(&dependencies));

        let own = CanonicalId::for_project(ProviderId::Modrinth, "candidate");
        let installed = |version_id: &str| {
            ManifestEntry::managed(
                CanonicalId::for_project(ProviderId::Modrinth, "installed"),
                ProviderId::Modrinth,
                "installed".to_string(),
                version_id.to_string(),
                ContentKind::Mod,
                &file("installed.jar", None),
                Vec::new(),
                None,
            )
        };
        let mut update = version(
            "candidate-v2",
            ReleaseChannel::Release,
            vec![file("candidate.jar", None)],
        );
        update.dependencies = dependencies;

        assert!(version_conflicts_with_installed(
            &update,
            &own,
            &[installed("installed-v1")],
        ));
        assert!(!version_conflicts_with_installed(
            &update,
            &own,
            &[installed("installed-v2")],
        ));
    }

    #[test]
    fn selected_content_is_checked_for_mutual_incompatibility() {
        let items = vec![
            resolved_item("first", Some("second")),
            resolved_item("second", None),
        ];

        let conflicts = selected_incompatibility_conflicts(&items);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind(), ResolutionConflictKind::Incompatible);
        assert_eq!(
            conflicts[0].reason,
            ResolutionConflictReason::SelectedIncompatibility {
                other_project_id: "second".to_string(),
            }
        );
        assert_eq!(
            conflicts[0].canonical_id.as_ref().map(CanonicalId::as_str),
            Some("modrinth:first")
        );
    }

    #[test]
    fn selected_incompatibilities_honor_exact_version_ids() {
        let mut nonmatching = resolved_item("first", Some("second"));
        nonmatching.dependencies[0].version_id = Some("different-version".to_string());
        assert!(
            selected_incompatibility_conflicts(&[nonmatching, resolved_item("second", None),])
                .is_empty()
        );

        let mut matching = resolved_item("first", Some("second"));
        matching.dependencies[0].version_id = Some("v1".to_string());
        assert_eq!(
            selected_incompatibility_conflicts(&[matching, resolved_item("second", None)]).len(),
            1
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
        entry.sha512 = Some(crate::sha512_file(&path).expect("managed hash"));
        entry.size = Some(3);
        assert_eq!(
            resolved_install_state(Some(&entry), Some(&root), "v1"),
            (true, false)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolution_to_install_includes_only_fresh_and_updated_items() {
        let resolved = |id: &str, size: u64, already: bool, update: bool| ResolvedContentItem {
            canonical_id: CanonicalId(format!("modrinth:{id}")),
            provider: ProviderId::Modrinth,
            project_id: id.to_string(),
            kind: ContentKind::Mod,
            version_id: format!("{id}-v"),
            version_number: "1".to_string(),
            title: id.to_string(),
            file: file(&format!("{id}.jar"), Some(size)),
            dependencies: Vec::new(),
            reason: ResolutionReason::Selected,
            already_installed: already,
            update,
        };
        let resolution = ContentResolution {
            items: vec![
                resolved("fresh", 100, false, false),
                resolved("update", 200, true, true),
                resolved("skip", 400, true, false),
            ],
            conflicts: Vec::new(),
        };

        let planned = resolution.to_install();

        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].project_id, "fresh");
        assert_eq!(planned[1].project_id, "update");
    }
}
