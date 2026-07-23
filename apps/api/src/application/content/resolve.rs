//! Application adapter for the core content dependency resolver. This module
//! keeps transport DTOs, public copy/redaction and execution error
//! classification at the API boundary while `axial-content` owns the graph.

use super::{
    ContentApiError, ContentExecutionError, ContentSelection, content_error_response,
    content_execution_error, json_error,
};
use crate::observability::{RedactionAudience, sanitize_public_diagnostic_text};
use crate::state::AppState;
use axial_content::{
    CanonicalId, ContentDependency, ContentKind, ContentResolution, LiveManagedContent,
    ResolutionConflict, ResolutionConflictKind, ResolutionConflictReason, ResolutionError,
    ResolutionLimitKind, ResolutionReason, ResolutionSelection, ResolutionTarget, resolve_content,
};
use axum::http::StatusCode;
use serde::Serialize;

const MAX_CONFLICT_LABEL_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReason {
    Selected,
    Dependency,
}

impl From<ResolutionReason> for PlanReason {
    fn from(reason: ResolutionReason) -> Self {
        match reason {
            ResolutionReason::Selected => Self::Selected,
            ResolutionReason::Dependency => Self::Dependency,
        }
    }
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

impl From<ResolutionConflictKind> for ConflictKind {
    fn from(kind: ResolutionConflictKind) -> Self {
        match kind {
            ResolutionConflictKind::Unavailable => Self::Unavailable,
            ResolutionConflictKind::Incompatible => Self::Incompatible,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PlanConflict {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<CanonicalId>,
    pub kind: ConflictKind,
    pub detail: String,
}

impl From<ResolutionConflict> for PlanConflict {
    fn from(conflict: ResolutionConflict) -> Self {
        let kind = conflict.kind().into();
        let detail = public_conflict_detail(&conflict);
        Self {
            canonical_id: conflict.canonical_id,
            kind,
            detail,
        }
    }
}

fn public_conflict_detail(conflict: &ResolutionConflict) -> String {
    if conflict.reason == ResolutionConflictReason::StabilizationFailed {
        return "could not stabilize exact dependency requirements".to_string();
    }

    let subject = bounded_conflict_label(conflict.subject_title.as_deref(), "This content");
    match &conflict.reason {
        ResolutionConflictReason::NoCompatibleVersion => {
            format!("{subject} has no compatible version for this loader and Minecraft version")
        }
        ResolutionConflictReason::RequiredDependencyUnidentified => {
            format!("{subject} has a required dependency that could not be identified")
        }
        ResolutionConflictReason::ExactVersionConflict { .. } => {
            format!("{subject} has conflicting exact version requirements")
        }
        ResolutionConflictReason::SelectedIncompatibility { .. } => {
            format!("{subject} is incompatible with other selected content")
        }
        ResolutionConflictReason::InstalledIncompatibility {
            installed_title, ..
        } => {
            let installed = bounded_conflict_label(installed_title.as_deref(), "installed content");
            format!("{subject} is incompatible with {installed}, which is already installed")
        }
        ResolutionConflictReason::StabilizationFailed => unreachable!("handled above"),
    }
}

fn bounded_conflict_label(value: Option<&str>, fallback: &str) -> String {
    value
        .map(|value| {
            sanitize_public_diagnostic_text(
                value,
                RedactionAudience::UserVisible,
                MAX_CONFLICT_LABEL_CHARS,
                "",
            )
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
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

pub async fn resolve(
    state: &AppState,
    target: &ResolutionTarget,
    selections: &[ContentSelection],
    live_content: &LiveManagedContent,
) -> Result<ContentResolution, ContentApiError> {
    resolve_core(state, target, selections, live_content)
        .await
        .map_err(resolution_error_response)
}

pub(crate) async fn resolve_for_execution(
    state: &AppState,
    target: &ResolutionTarget,
    selections: &[ContentSelection],
    live_content: &LiveManagedContent,
) -> Result<ContentResolution, ContentExecutionError> {
    resolve_core(state, target, selections, live_content)
        .await
        .map_err(resolution_execution_error)
}

async fn resolve_core(
    state: &AppState,
    target: &ResolutionTarget,
    selections: &[ContentSelection],
    live_content: &LiveManagedContent,
) -> Result<ContentResolution, ResolutionError> {
    let selections: Vec<ResolutionSelection> = selections
        .iter()
        .map(|selection| ResolutionSelection {
            canonical_id: selection.canonical_id.clone(),
            kind: selection.kind,
            version_id: selection.version_id.clone(),
        })
        .collect();
    resolve_content(state.content(), target, &selections, live_content).await
}

pub fn into_plan(
    resolution: ContentResolution,
    instance_id: Option<String>,
    target: &ResolutionTarget,
) -> ResolutionPlan {
    let total_download_bytes = resolution
        .items
        .iter()
        .filter(|item| !item.already_installed || item.update)
        .filter_map(|item| item.file.size)
        .sum();
    let items = resolution
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
            reason: item.reason.into(),
            already_installed: item.already_installed,
            update: item.update,
        })
        .collect();
    ResolutionPlan {
        instance_id,
        loader: target.loader.clone(),
        game_version: target.game_version.clone(),
        items,
        conflicts: resolution
            .conflicts
            .into_iter()
            .map(PlanConflict::from)
            .collect(),
        total_download_bytes,
    }
}

fn resolution_error_response(error: ResolutionError) -> ContentApiError {
    match error {
        ResolutionError::NoSelection => json_error(StatusCode::BAD_REQUEST, "no content selected"),
        ResolutionError::SelectedKindChanged => json_error(
            StatusCode::BAD_REQUEST,
            "the selected content type changed; refresh it and try again",
        ),
        ResolutionError::DuplicateSelection => json_error(
            StatusCode::BAD_REQUEST,
            "the same content cannot be selected more than once",
        ),
        ResolutionError::ModpackRequiresInstance => json_error(
            StatusCode::BAD_REQUEST,
            "a modpack is installed as its own instance, not added to one",
        ),
        ResolutionError::ModLoaderRequired => json_error(
            StatusCode::PRECONDITION_FAILED,
            "this instance has no mod loader; add mods to a modded instance",
        ),
        ResolutionError::InstalledDependencyUnidentified => json_error(
            StatusCode::CONFLICT,
            "an installed exact dependency could not be identified",
        ),
        ResolutionError::LimitExceeded(limit) => match limit.kind {
            ResolutionLimitKind::Selections => json_error(
                StatusCode::BAD_REQUEST,
                "too many content items were selected at once",
            ),
            ResolutionLimitKind::Nodes
            | ResolutionLimitKind::Depth
            | ResolutionLimitKind::DependenciesPerNode
            | ResolutionLimitKind::Edges
            | ResolutionLimitKind::Queue
            | ResolutionLimitKind::Conflicts
            | ResolutionLimitKind::GraphBytes
            | ResolutionLimitKind::StructuredOutput => json_error(
                StatusCode::CONFLICT,
                "the content dependency graph exceeds its safety limits",
            ),
        },
        ResolutionError::Provider(error) => content_error_response(error),
    }
}

fn resolution_execution_error(error: ResolutionError) -> ContentExecutionError {
    match error {
        ResolutionError::Provider(error) => content_execution_error(error),
        error => resolution_error_response(error).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::ContentExecutionFailureKind;
    use super::*;
    use axial_content::{ContentError, FileRef, ProviderId, ResolvedContentItem};

    fn conflict(
        subject_title: Option<&str>,
        reason: ResolutionConflictReason,
    ) -> ResolutionConflict {
        ResolutionConflict {
            canonical_id: Some(CanonicalId::for_project(ProviderId::Modrinth, "subject")),
            subject_title: subject_title.map(str::to_string),
            reason,
        }
    }

    fn planned_item(size: u64, already_installed: bool, update: bool) -> ResolvedContentItem {
        ResolvedContentItem {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, "project"),
            provider: ProviderId::Modrinth,
            project_id: "project".to_string(),
            kind: ContentKind::Mod,
            version_id: "version".to_string(),
            version_number: "1.0".to_string(),
            title: "Project".to_string(),
            file: FileRef {
                url: "https://example.invalid/project.jar".to_string(),
                filename: "project.jar".to_string(),
                sha1: Some("a".repeat(40)),
                sha512: Some("b".repeat(128)),
                size: Some(size),
                primary: true,
            },
            dependencies: Vec::new(),
            reason: ResolutionReason::Selected,
            already_installed,
            update,
        }
    }

    #[test]
    fn execution_resolution_preserves_closed_provider_failure_kinds() {
        let cases = [
            (
                ContentError::DownloadPreparation("prepare download".to_string()),
                ContentExecutionFailureKind::NetworkFailure,
            ),
            (
                ContentError::Status {
                    status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    context: "resolve versions".to_string(),
                },
                ContentExecutionFailureKind::ProviderFailure,
            ),
            (
                ContentError::ProviderMetadataInvalid("invalid versions".to_string()),
                ContentExecutionFailureKind::MetadataInvalid,
            ),
        ];

        for (error, expected) in cases {
            let (_, failure_kind) =
                resolution_execution_error(ResolutionError::Provider(error)).into_parts();
            assert_eq!(failure_kind, Some(expected));
        }
    }

    #[test]
    fn execution_resolution_leaves_local_conflicts_unclassified() {
        let (_, failure_kind) =
            resolution_execution_error(ResolutionError::NoSelection).into_parts();

        assert_eq!(failure_kind, None);
    }

    #[test]
    fn conflict_projection_uses_fixed_copy_without_echoing_provider_identifiers() {
        let cases = [
            (
                conflict(
                    Some("Sodium"),
                    ResolutionConflictReason::NoCompatibleVersion,
                ),
                ConflictKind::Unavailable,
                "Sodium has no compatible version for this loader and Minecraft version",
            ),
            (
                conflict(
                    None,
                    ResolutionConflictReason::RequiredDependencyUnidentified,
                ),
                ConflictKind::Unavailable,
                "This content has a required dependency that could not be identified",
            ),
            (
                ResolutionConflict {
                    canonical_id: None,
                    subject_title: None,
                    reason: ResolutionConflictReason::StabilizationFailed,
                },
                ConflictKind::Unavailable,
                "could not stabilize exact dependency requirements",
            ),
            (
                conflict(
                    None,
                    ResolutionConflictReason::ExactVersionConflict {
                        chosen_version_id: "provider-secret-version-a".to_string(),
                        required_version_id: "provider-secret-version-b".to_string(),
                    },
                ),
                ConflictKind::Unavailable,
                "This content has conflicting exact version requirements",
            ),
            (
                conflict(
                    None,
                    ResolutionConflictReason::SelectedIncompatibility {
                        other_project_id: "provider-secret-project".to_string(),
                    },
                ),
                ConflictKind::Incompatible,
                "This content is incompatible with other selected content",
            ),
            (
                conflict(
                    Some("Iris"),
                    ResolutionConflictReason::InstalledIncompatibility {
                        installed_project_id: "provider-secret-project".to_string(),
                        installed_title: Some("OptiFine".to_string()),
                    },
                ),
                ConflictKind::Incompatible,
                "Iris is incompatible with OptiFine, which is already installed",
            ),
        ];

        for (conflict, expected_kind, expected_detail) in cases {
            let projected = PlanConflict::from(conflict);
            assert_eq!(projected.kind, expected_kind);
            assert_eq!(projected.detail, expected_detail);
            assert!(!projected.detail.contains("provider-secret"));
        }
    }

    #[test]
    fn conflict_projection_bounds_and_redacts_provider_titles() {
        let unsafe_conflict = conflict(
            Some("/tmp/private/account token.jar"),
            ResolutionConflictReason::InstalledIncompatibility {
                installed_project_id: "opaque-project".to_string(),
                installed_title: Some("C:\\Users\\private\\secret.jar".to_string()),
            },
        );
        let unsafe_projected = PlanConflict::from(unsafe_conflict);
        assert_eq!(
            unsafe_projected.detail,
            "This content is incompatible with installed content, which is already installed"
        );

        let oversized = "A".repeat(MAX_CONFLICT_LABEL_CHARS * 4);
        let bounded = PlanConflict::from(conflict(
            Some(&oversized),
            ResolutionConflictReason::NoCompatibleVersion,
        ));
        assert_eq!(
            bounded
                .detail
                .chars()
                .take(MAX_CONFLICT_LABEL_CHARS)
                .count(),
            MAX_CONFLICT_LABEL_CHARS
        );
        assert!(!bounded.detail.contains(&oversized));
        assert!(bounded.detail.chars().count() < 180);
    }

    #[test]
    fn limit_projection_uses_fixed_copy_without_structured_fact_values() {
        let (status, body) = resolution_error_response(ResolutionError::LimitExceeded(
            axial_content::ResolutionLimitExceeded {
                kind: ResolutionLimitKind::Edges,
                maximum: 4096,
                observed: 987_654,
            },
        ));
        let body = body.0;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some("the content dependency graph exceeds its safety limits")
        );
        assert!(!body.to_string().contains("987654"));
    }

    #[test]
    fn plan_projection_preserves_fields_and_counts_only_mutated_bytes() {
        let resolution = ContentResolution {
            items: vec![
                planned_item(100, false, false),
                planned_item(200, true, true),
                planned_item(400, true, false),
            ],
            conflicts: vec![conflict(
                Some("Project"),
                ResolutionConflictReason::NoCompatibleVersion,
            )],
        };
        let target = ResolutionTarget {
            loader: "fabric".to_string(),
            game_version: "1.21.11".to_string(),
            supports_mods: true,
        };

        let plan = into_plan(resolution, Some("instance".to_string()), &target);

        assert_eq!(plan.instance_id.as_deref(), Some("instance"));
        assert_eq!(plan.loader, "fabric");
        assert_eq!(plan.game_version, "1.21.11");
        assert_eq!(plan.total_download_bytes, 300);
        assert_eq!(plan.items.len(), 3);
        assert_eq!(plan.items[0].canonical_id.as_str(), "modrinth:project");
        assert_eq!(plan.items[0].title, "Project");
        assert_eq!(plan.items[0].filename, "project.jar");
        assert_eq!(
            plan.items[0].sha1.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            plan.items[0].sha512.as_deref(),
            Some(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )
        );
        assert_eq!(plan.items[0].reason, PlanReason::Selected);
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].kind, ConflictKind::Unavailable);
        assert_eq!(
            plan.conflicts[0].detail,
            "Project has no compatible version for this loader and Minecraft version"
        );
    }
}
