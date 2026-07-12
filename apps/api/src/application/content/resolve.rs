//! Backend-authored resolution: expand required dependencies, flag conflicts
//! against what the target already has, and pick a concrete version and file for
//! every item. The client never chooses a download URL; it only echoes back
//! selections, and this pass re-derives everything server-side.

use super::target::ResolveTarget;
use super::{ContentApiError, ContentSelection, content_error_response, json_error};
use axial_content::{
    CanonicalId, ContentDependency, ContentError, ContentKind, ContentManifest, ContentVersion,
    DependencyKind, FileRef, PlannedFile, ProviderId, ReleaseChannel,
};
use axum::http::StatusCode;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};

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
                size: item.file.size,
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

/// Reject selections the target cannot physically accept. Resource packs and
/// shaders drop into any instance; a mod needs a loader.
pub fn require_installable(
    selections: &[ContentSelection],
    target: &ResolveTarget,
) -> Result<(), ContentApiError> {
    if selections.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "no content selected"));
    }
    if selections
        .iter()
        .any(|selection| selection.kind == ContentKind::Modpack)
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "a modpack is installed as its own instance, not added to one",
        ));
    }
    if !target.supports_mods
        && selections
            .iter()
            .any(|selection| selection.kind.requires_mod_loader())
    {
        return Err(json_error(
            StatusCode::PRECONDITION_FAILED,
            "this instance has no mod loader; add mods to a modded instance",
        ));
    }
    Ok(())
}

pub async fn resolve(
    state: &AppState,
    target: &ResolveTarget,
    selections: &[ContentSelection],
    manifest: &ContentManifest,
) -> Result<Resolution, ContentApiError> {
    let mut resolved_ids: HashSet<CanonicalId> = HashSet::new();
    let mut items: Vec<ResolvedItem> = Vec::new();
    let mut conflicts: Vec<PlanConflict> = Vec::new();

    let mut queue: VecDeque<(CanonicalId, ContentKind, Option<String>, PlanReason)> = selections
        .iter()
        .map(|selection| {
            (
                CanonicalId(selection.canonical_id.clone()),
                selection.kind,
                selection.version_id.clone(),
                PlanReason::Selected,
            )
        })
        .collect();

    while let Some((canonical_id, kind, forced_version, reason)) = queue.pop_front() {
        if !resolved_ids.insert(canonical_id.clone()) {
            continue;
        }
        if items.len() >= MAX_RESOLVE_ITEMS {
            break;
        }

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

        for dependency in &version.dependencies {
            match dependency.kind {
                DependencyKind::Required => {
                    if let Some(project_id) = &dependency.project_id {
                        // A dependency of a mod is a mod; nothing else declares
                        // required dependencies upstream.
                        queue.push_back((
                            CanonicalId::for_project(ProviderId::Modrinth, project_id),
                            kind,
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
            kind,
            version_id: version.id.clone(),
            version_number: version.version_number.clone(),
            // A placeholder: a version is named things like "Sodium 0.7.3 for
            // Fabric 1.21.8", which is not what anyone calls the mod. The real
            // project titles are fetched in one batch below.
            title: version.name.clone(),
            file,
            dependencies: version.dependencies.clone(),
            reason,
            already_installed,
            update,
        });
    }

    apply_project_titles(state, &mut items).await;
    Ok(Resolution { items, conflicts })
}

/// Replace version names with project names in one round trip. Best-effort: if
/// the lookup fails the plan still stands, it just reads worse.
async fn apply_project_titles(state: &AppState, items: &mut [ResolvedItem]) {
    if items.is_empty() {
        return;
    }
    let ids: Vec<CanonicalId> = items.iter().map(|item| item.canonical_id.clone()).collect();
    let Ok(titles) = state.content().titles(&ids).await else {
        return;
    };
    for item in items {
        if let Some(title) = titles.get(&item.canonical_id) {
            item.title = title.clone();
        }
    }
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
        detail: "no compatible version for this loader and Minecraft version".to_string(),
    }
}

fn incompatible_conflict(canonical_id: &CanonicalId) -> PlanConflict {
    PlanConflict {
        canonical_id: Some(canonical_id.clone()),
        kind: ConflictKind::Incompatible,
        detail: "conflicts with content already installed in this instance".to_string(),
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
    fn a_vanilla_target_takes_packs_but_refuses_mods() {
        let vanilla = target(false);

        for kind in [ContentKind::ResourcePack, ContentKind::ShaderPack] {
            assert!(
                require_installable(&[selection("modrinth:x", kind)], &vanilla).is_ok(),
                "{kind:?} needs no mod loader"
            );
        }
        assert!(
            require_installable(&[selection("modrinth:x", ContentKind::Mod)], &vanilla).is_err()
        );
    }

    #[test]
    fn a_modded_target_takes_every_installable_kind() {
        let fabric = target(true);
        for kind in [
            ContentKind::Mod,
            ContentKind::ResourcePack,
            ContentKind::ShaderPack,
        ] {
            assert!(require_installable(&[selection("modrinth:x", kind)], &fabric).is_ok());
        }
    }

    #[test]
    fn modpacks_are_never_added_to_an_instance() {
        assert!(
            require_installable(
                &[selection("modrinth:x", ContentKind::Modpack)],
                &target(true)
            )
            .is_err()
        );
        assert!(require_installable(&[], &target(true)).is_err());
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
