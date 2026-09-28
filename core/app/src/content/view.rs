//! Retained content preview shapes shared by discovery and instance setup.

use super::{
    catalog::ContentService,
    model::{CanonicalId, ContentDependency, ContentKind},
    provenance::{ContentManifest, LiveManagedContent},
    resolve::{
        self, ContentResolution, ResolutionConflictKind, ResolutionConflictReason, ResolutionError,
        ResolutionReason, ResolutionSelection, ResolutionTarget,
    },
};
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
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanConflict {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<CanonicalId>,
    pub kind: ResolutionConflictKind,
    pub detail: String,
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
            })
            .collect(),
        conflicts: resolution
            .conflicts
            .iter()
            .map(|conflict| PlanConflict {
                canonical_id: conflict.canonical_id.clone(),
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
            })
            .collect(),
    }
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
