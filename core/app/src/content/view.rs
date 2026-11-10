//! Retained content preview shapes shared by discovery and instance setup.

use super::{
    catalog::{ContentResult, ContentService},
    install::ContentMutations,
    model::{
        CanonicalContent, CanonicalId, ContentDependency, ContentKind, ContentQuery, Page,
        ProviderId,
    },
    provenance::{ContentManifest, LiveManagedContent},
    resolve::{
        self, ContentResolution, ResolutionConflictKind, ResolutionConflictReason, ResolutionError,
        ResolutionReason, ResolutionSelection, ResolutionTarget, TargetedPlan,
    },
};
use crate::instances::model::InstanceId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetRef {
    Instance {
        instance_id: String,
    },
    Draft {
        #[serde(default)]
        loader: Option<String>,
        game_version: String,
    },
}

#[derive(Clone, Debug, Serialize)]
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<ContentDependency>,
    pub reason: ResolutionReason,
    pub already_installed: bool,
    pub update: bool,
    /// An untracked file already at this destination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_file: Option<ExistingFileState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExistingFileState {
    /// Same bytes as the planned file; it is adopted without downloading.
    Identical,
    /// Different bytes; installing requires replacing it.
    Different,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanConflict {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<CanonicalId>,
    /// The project the conflict is about, when its metadata was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub kind: ResolutionConflictKind,
    pub detail: String,
    /// Instance-relative file a replacement decision names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolutionPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub loader: String,
    pub game_version: String,
    pub items: Vec<PlanItem>,
    pub conflicts: Vec<PlanConflict>,
    pub total_download_bytes: u64,
}

pub fn into_plan(
    resolution: &ContentResolution,
    instance_id: Option<String>,
    target: &ResolutionTarget,
) -> ResolutionPlan {
    ResolutionPlan {
        instance_id,
        loader: target.loader.clone(),
        game_version: target.game_version.clone(),
        total_download_bytes: resolution
            .items
            .iter()
            .filter(|item| !item.already_installed || item.update)
            .filter_map(|item| item.file.size)
            .sum(),
        items: resolution
            .items
            .iter()
            .map(|item| PlanItem {
                canonical_id: item.canonical_id.clone(),
                title: item.title.clone(),
                kind: item.kind,
                project_id: item.project_id.clone(),
                version_id: item.version_id.clone(),
                version_number: item.version_number.clone(),
                filename: item.file.filename.clone(),
                sha1: item.file.sha1.clone(),
                sha512: item.file.sha512.clone(),
                size: item.file.size,
                dependencies: item.dependencies.clone(),
                reason: item.reason,
                already_installed: item.already_installed,
                update: item.update,
                existing_file: None,
            })
            .collect(),
        conflicts: resolution
            .conflicts
            .iter()
            .map(|conflict| PlanConflict {
                canonical_id: conflict.canonical_id.clone(),
                title: conflict.subject_title.clone().or_else(|| {
                    let id = conflict.canonical_id.as_ref()?;
                    resolution
                        .items
                        .iter()
                        .find(|item| &item.canonical_id == id)
                        .map(|item| item.title.clone())
                }),
                kind: conflict.kind(),
                detail: match conflict.reason {
                    ResolutionConflictReason::NoCompatibleVersion => {
                        "Content has no compatible version for this loader and Minecraft version."
                    }
                    ResolutionConflictReason::RequiredDependencyUnidentified => {
                        "A required dependency could not be identified."
                    }
                    ResolutionConflictReason::StabilizationFailed => {
                        "Exact dependency requirements could not be stabilized."
                    }
                    ResolutionConflictReason::ExactVersionConflict { .. } => {
                        "Content has conflicting exact version requirements."
                    }
                    ResolutionConflictReason::SelectedIncompatibility { .. } => {
                        "Content is incompatible with other selected content."
                    }
                    ResolutionConflictReason::InstalledIncompatibility { .. } => {
                        "Content is incompatible with already installed content."
                    }
                }
                .into(),
                path: None,
            })
            .collect(),
    }
}

/// An instance plan also reports untracked files at its destinations: equal
/// bytes are adopted, different bytes need an explicit replacement choice.
pub fn into_instance_plan(plan: &TargetedPlan, instance_id: String) -> ResolutionPlan {
    let mut view = into_plan(plan.resolution(), Some(instance_id), plan.state().target());
    for existing in plan.existing_files() {
        let Some(item) = view
            .items
            .iter_mut()
            .find(|item| item.canonical_id == existing.canonical_id)
        else {
            continue;
        };
        if existing.identical {
            if item.existing_file.is_none() {
                item.existing_file = Some(ExistingFileState::Identical);
                view.total_download_bytes = view
                    .total_download_bytes
                    .saturating_sub(item.size.unwrap_or(0));
            }
            continue;
        }
        item.existing_file = Some(ExistingFileState::Different);
        let (folder, name) = existing
            .path
            .rsplit_once('/')
            .unwrap_or(("", existing.path.as_str()));
        let title = Some(item.title.clone());
        view.conflicts.push(PlanConflict {
            canonical_id: Some(existing.canonical_id.clone()),
            title,
            kind: ResolutionConflictKind::ExistingFile,
            detail: format!("A different {name} is already in the {folder} folder."),
            path: Some(existing.path.clone()),
        });
    }
    view
}

pub async fn preview_draft(
    service: &ContentService,
    target: &ResolutionTarget,
    selections: &[ResolutionSelection],
) -> Result<ContentResolution, ResolutionError> {
    resolve::resolve_content(
        service,
        target,
        selections,
        &ContentManifest::default(),
        &LiveManagedContent::default(),
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Installed,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchItem {
    #[serde(flatten)]
    pub content: CanonicalContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_state: Option<InstallState>,
}

/// A provider page marking results the instance has live. An unreadable or
/// unknown instance only omits the marks; discovery stays available.
pub async fn search(
    service: &ContentService,
    mutations: &ContentMutations,
    query: &ContentQuery,
    instance: Option<&InstanceId>,
) -> ContentResult<Page<SearchItem>> {
    let context = instance.and_then(|id| mutations.instance_context(id).ok());
    let installed = |content: &CanonicalContent| {
        context.as_ref().is_some_and(|(_, manifest, live)| {
            manifest
                .find(&content.canonical_id)
                .is_some_and(|entry| live.contains(entry))
        })
    };
    let page = service.search(query).await?;
    Ok(Page {
        items: page
            .items
            .into_iter()
            .map(|content| SearchItem {
                install_state: installed(&content).then_some(InstallState::Installed),
                content,
            })
            .collect(),
        offset: page.offset,
        limit: page.limit,
        total: page.total,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledContent {
    pub entries: Vec<InstalledEntry>,
    pub packs: Vec<InstalledPack>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledEntry {
    pub canonical_id: CanonicalId,
    pub kind: ContentKind,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_by: Vec<CanonicalId>,
}

/// Pack provenance owns no file; its recorded paths only mark membership.
#[derive(Clone, Debug, Serialize)]
pub struct InstalledPack {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_number: Option<String>,
}

/// Live managed files and the packs that placed them.
pub fn installed(manifest: &ContentManifest, live: &LiveManagedContent) -> InstalledContent {
    let entries = manifest
        .entries()
        .iter()
        .filter(|entry| live.contains(entry))
        .filter_map(|entry| {
            Some(InstalledEntry {
                canonical_id: entry.canonical_id().clone(),
                kind: entry.kind(),
                provider: entry.provider(),
                project_id: entry.project_id().into(),
                version_id: entry.version_id().into(),
                filename: entry.managed_filename()?.as_str().into(),
                enabled: entry.enabled(),
                title: entry.title().map(Into::into),
                required_by: manifest
                    .dependents(live, std::slice::from_ref(entry.canonical_id()))
                    .into_iter()
                    .map(|dependent| dependent.canonical_id().clone())
                    .collect(),
            })
        })
        .collect();
    let packs = manifest
        .entries()
        .iter()
        .filter(|entry| entry.kind() == ContentKind::Modpack)
        .map(|entry| InstalledPack {
            canonical_id: entry.canonical_id().clone(),
            provider: entry.provider(),
            project_id: entry.project_id().into(),
            version_id: entry.version_id().into(),
            paths: entry.pack_paths().map(Into::into).collect(),
            title: entry.title().map(Into::into),
            version_number: entry.version_number().map(Into::into),
        })
        .collect();
    InstalledContent { entries, packs }
}

#[derive(Clone, Debug, Serialize)]
pub struct CompatibilityResponse {
    pub candidates: Vec<super::compatibility::CompatCandidate>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpdatesResponse {
    pub updates: Vec<resolve::AvailableUpdate>,
}
